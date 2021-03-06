use std::borrow::Borrow;
use std::marker::PhantomData;

use crate::model::{eval_order, Model, TensorInfo};
use crate::ops::prelude::*;

#[derive(Debug, Default)]
pub struct SessionState {
    pub known_stream_len: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SimplePlan<TI:TensorInfo, M: Borrow<Model<TI>>> {
    pub model: M,
    pub order: Vec<usize>,
    pub flush_lists: Vec<TVec<usize>>,
    _casper: PhantomData<TI>,
}

impl<TI: TensorInfo, M: Borrow<Model<TI>>> SimplePlan<TI, M> {
    pub fn new(model: M) -> TractResult<SimplePlan<TI, M>> {
        let order = eval_order(model.borrow())?;
        let mut values_needed_until_step = vec![0; model.borrow().nodes().len()];
        for step in 0..order.len() {
            for i in &model.borrow().node(order[step]).inputs {
                values_needed_until_step[i.node] = step;
            }
        }
        for o in model.borrow().outputs()? {
            values_needed_until_step[o.node] = order.len();
        }
        let mut flush_lists: Vec<TVec<usize>> = vec![tvec!(); order.len() + 1];
        for (node, &flush_at) in values_needed_until_step.iter().enumerate() {
            if flush_at != 0 {
                flush_lists[flush_at].push(node)
            }
        }
        Ok(SimplePlan {
            model,
            order,
            flush_lists,
            _casper: PhantomData,
        })
    }

    pub fn run(&self, inputs: TVec<Tensor>) -> TractResult<TVec<SharedTensor>> {
        let mut state = SimpleState::new(self)?;
        state.run(inputs)
    }

    pub fn model(&self) -> &Model<TI> {
        self.model.borrow()
    }
}

#[derive(Debug)]
pub struct SimpleState<TI: TensorInfo, M: Borrow<Model<TI>>, P: Borrow<SimplePlan<TI, M>>> {
    plan: P,
    pub states: Vec<Option<Box<OpState>>>,
    pub session_state: SessionState,
    pub values: Vec<Option<TVec<SharedTensor>>>,
    _phantom: PhantomData<(M,TI)>,
}

impl<TI:TensorInfo, M: Borrow<Model<TI>>, P: Borrow<SimplePlan<TI, M>> + Clone> Clone for SimpleState<TI, M, P> {
    fn clone(&self) -> SimpleState<TI, M, P> {
        let states = self
            .states
            .iter()
            .map(|opt: &Option<Box<OpState>>| -> Option<Box<OpState>> {
                opt.as_ref().map(|b| ::objekt::clone_box(&**b))
            })
            .collect();
        SimpleState {
            plan: self.plan.clone(),
            states,
            session_state: SessionState::default(),
            values: self.values.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<TI: TensorInfo, M: Borrow<Model<TI>>, P: Borrow<SimplePlan<TI, M>>> SimpleState<TI, M, P> {
    pub fn new(plan: P) -> TractResult<SimpleState<TI, M, P>> {
        let values = vec![None; plan.borrow().model.borrow().nodes().len()];
        let states = plan
            .borrow()
            .model()
            .nodes()
            .iter()
            .map(|n| n.op().state())
            .collect::<TractResult<_>>()?;
        Ok(SimpleState {
            states,
            session_state: SessionState::default(),
            plan,
            values,
            _phantom: PhantomData,
        })
    }

    /// Reset wires state.
    pub fn reset_wires(&mut self) -> TractResult<()> {
        self.values.iter_mut().for_each(|s| *s = None);
        Ok(())
    }

    /// Reset wires state.
    pub fn reset_op_states(&mut self) -> TractResult<()> {
        self.states = self
            .plan
            .borrow()
            .model()
            .nodes()
            .iter()
            .map(|n| n.op().state())
            .collect::<TractResult<_>>()?;
        Ok(())
    }

    pub fn run(&mut self, inputs: TVec<Tensor>) -> TractResult<TVec<SharedTensor>> {
        use crate::ops::source::Source;
        let mut result = tvec!();
        {
            let &mut SimpleState {
                ref plan,
                ref mut session_state,
                ref mut states,
                ref mut values,
                ..
            } = self;
            let model = plan.borrow().model();
            for (input, v) in model.inputs()?.iter().zip(inputs.into_iter()) {
                values[input.node] = Some(tvec!(v.into()));
            }
            let plan = plan.borrow();
            for (step, n) in plan.order.iter().enumerate() {
                let node = model.node(*n);
                trace!("Running step {}, node {}", step, node);
                if node.op_as::<Source>().is_none() {
                    let mut inputs: TVec<SharedTensor> = tvec![];
                    for i in &node.inputs {
                        trace!("  use input {:?}", i);
                        let prec_node = model.node(i.node);
                        let prec = values[i.node].as_ref().ok_or_else(|| {
                            format!(
                                "Computing {}, precursor {} not done:",
                                node, prec_node
                            )
                        })?;
                        inputs.push(prec[i.slot].clone().into())
                    }
                    let vs = match states[node.id] {
                        Some(ref mut state) => state.eval(session_state, node.op(), inputs),
                        None => node.op().as_stateless().unwrap().eval(inputs),
                    }
                    .map_err(|e| format!("Evaluating {}: {}", node, e))?;

                    values[node.id] = Some(vs);
                }
                for flush in &plan.flush_lists[step] {
                    trace!("  flushing node {} {}", flush, node);
                    values[*flush] = None;
                }
            }
            for output in model.outputs()? {
                result.push(values[output.node].as_ref().unwrap()[output.slot].clone())
            }
        }
        self.reset_wires()?;
        Ok(result)
    }

    pub fn set_inputs(&mut self, inputs: TVec<Tensor>) -> TractResult<()> {
        let SimpleState {
            ref plan,
            ref mut values,
            ..
        } = self;
        plan.borrow()
            .model()
            .inputs()?
            .iter()
            .zip(inputs)
            .for_each(|(input, t)| values[input.node] = Some(tvec![t.into()]));
        Ok(())
    }

    pub fn set_input(&mut self, input: usize, t: Tensor) -> TractResult<()> {
        let id = self.model().inputs()?[input].node;
        self.values[id] = Some(tvec![t.into()]);
        Ok(())
    }

    pub fn take_outputs(&mut self) -> TractResult<Vec<SharedTensor>> {
        let SimpleState {
            ref plan,
            ref mut values,
            ..
        } = self;
        let mut v = vec![];
        for o in plan.borrow().model().outputs()?.iter() {
            let vs = values[o.node].as_mut().ok_or_else(|| {
                format!(
                    "SharedTensor for {:?} is not computed",
                    &plan.borrow().model().nodes()[o.node]
                )
            })?;
            v.push(vs[o.slot].clone())
        }
        Ok(v)
    }

    pub fn set_values(&mut self, id: usize, values: TVec<Tensor>) -> TractResult<()> {
        self.values[id] = Some(values.into_iter().map(|t| t.into()).collect());
        Ok(())
    }

    pub fn set_value(&mut self, id: usize, value: Tensor) -> TractResult<()> {
        self.set_values(id, tvec!(value))
    }

    pub fn compute_one(&mut self, node: usize) -> TractResult<()> {
        let SimpleState {
            ref plan,
            ref mut session_state,
            ref mut values,
            ..
        } = self;
        let plan = plan.borrow();
        let nodes = plan.model().nodes();
        let node = &nodes[node];
        let mut inputs: TVec<SharedTensor> = tvec![];
        for i in &node.inputs {
            let prec_node = &nodes[i.node];
            let prec = values[i.node].as_ref().ok_or_else(|| {
                format!(
                    "Computing {}, precursor {} not done.",
                    node, prec_node
                )
            })?;
            inputs.push(prec[i.slot].clone().into())
        }
        let vs = match self.states[node.id] {
            Some(ref mut state) => state.eval(session_state, node.op(), inputs),
            None => node.op().as_stateless().unwrap().eval(inputs),
        }
        .map_err(|e| format!("Evaluating {}: {}", node, e))?;
        values[node.id] = Some(vs);
        Ok(())
    }

    pub fn compute_recursively(&mut self, node: usize) -> TractResult<()> {
        let values = {
            let precs: Vec<usize> = self.model().nodes()[node]
                .inputs
                .iter()
                .map(|i| i.node)
                .collect();
            for i in precs.into_iter() {
                if self.values[i].is_none() {
                    self.compute_recursively(i)?
                }
            }
            let mut inputs: TVec<SharedTensor> = tvec![];
            {
                let node = &self.model().nodes()[node];
                for i in &node.inputs {
                    inputs.push(self.values[i.node].as_ref().unwrap()[i.slot].clone().into())
                }
            }
            let Self {
                ref mut states,
                ref mut session_state,
                ref plan,
                ..
            } = self;
            match states[node] {
                Some(ref mut state) => state.eval(session_state, plan.borrow().model().nodes()[node].op(), inputs),
                None => plan.borrow().model().nodes()[node]
                    .op()
                    .as_stateless()
                    .unwrap()
                    .eval(inputs),
            }
            .map_err(|e| format!("Evaluating {:?}: {:?}", node, e))?
        };
        self.values[node] = Some(values);
        Ok(())
    }

    pub fn take_by_name(&mut self, name: &str) -> TractResult<TVec<Tensor>> {
        let id = self.model().node_by_name(name)?.id;
        Self::take(self, id)
    }

    pub fn take(&mut self, id: usize) -> TractResult<TVec<Tensor>> {
        Ok(self.values[id]
            .take()
            .ok_or("SharedTensor is not computed")?
            .into_iter()
            .map(|v| v.to_tensor())
            .collect())
    }

    pub fn plan(&self) -> &SimplePlan<TI, M> {
        &self.plan.borrow()
    }

    pub fn model(&self) -> &Model<TI> {
        self.plan().model()
    }
}
