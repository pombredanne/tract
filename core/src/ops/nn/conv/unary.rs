use ndarray::*;

use crate::model::*;
use crate::ops::prelude::*;
use insideout::InsideOut;

use super::conv_gemm::ConvGemm;
use super::im2col::Im2Col;
use super::Conv;
use crate::ops::nn::conv::KernelFormat;
use crate::ops::nn::{DataFormat, PaddingSpec, Patch};

use std::sync::Arc;

use tract_linalg::MatMul;

#[derive(Debug, Clone)]
pub struct ConvUnary {
    pub data_fmt: DataFormat,
    pub kernel_fmt: KernelFormat,
    pub padding: PaddingSpec,
    pub dilations: TVec<usize>,
    pub strides: TVec<usize>,
    pub kernel: Tensor,

    pub bias: Option<Tensor>,
    pub full_input_shape: TVec<TDim>,
    pub full_output_shape: TVec<TDim>,
    pub group: usize,
}

impl ConvUnary {
    pub fn new(
        conv: &Conv,
        full_input_shape: &[TDim],
        full_output_shape: &[TDim],
        kernel: Tensor,
        bias: Option<Tensor>,
        group: usize,
    ) -> TractResult<ConvUnary> {
        let spatial_rank = full_input_shape.len() - 2;
        let dilations =
            conv.dilations.as_ref().map(|a| TVec::from(&**a)).unwrap_or(tvec!(1; spatial_rank));
        let strides =
            conv.strides.as_ref().map(|a| TVec::from(&**a)).unwrap_or(tvec!(1; spatial_rank));

        let unary = ConvUnary {
            data_fmt: conv.data_fmt,
            kernel_fmt: conv.kernel_fmt,
            padding: conv.padding.clone(),
            dilations,
            strides,
            kernel,
            bias,
            full_input_shape: full_input_shape.into(),
            full_output_shape: full_output_shape.into(),
            group,
        };
        Ok(unary)
    }

    fn patch(&self, input_full_shape: &[usize]) -> Patch {
        let kernel_spatial_shape =
            &self.kernel.shape()[self.kernel_fmt.h_axis()..][..(input_full_shape.len() - 2)];

        trace!("kernel spatial shape {:?}", kernel_spatial_shape);

        Patch::new(
            self.data_fmt,
            self.dilations.clone(),
            kernel_spatial_shape.into(),
            &self.padding,
            self.strides.clone(),
            input_full_shape.into(),
        )
    }

    fn input_channels(&self) -> usize {
        match self.kernel_fmt {
            KernelFormat::OIHW => self.kernel.shape()[1],
            KernelFormat::HWIO => self.kernel.shape()[self.kernel.shape().len() - 2],
        }
    }

    fn output_channels(&self) -> usize {
        match self.kernel_fmt {
            KernelFormat::OIHW => self.kernel.shape()[0],
            KernelFormat::HWIO => *self.kernel.shape().last().unwrap(),
        }
    }

    pub fn to_direct(&self, input_full_shape: &[usize]) -> TractResult<super::Direct> {
        assert!(
            (0..input_full_shape.len() - 2).all(|ax| self.padding.valid_dim(ax))
                && self.group == 1
                && self.bias.is_none()
        );

        let patch = self.patch(input_full_shape);
        let ref input_spatial_dims_strides: TVec<usize> = patch
            .input_shape
            .hw_axes()
            .map(|ax| input_full_shape.iter().skip(1 + ax).cloned().product::<usize>())
            .collect();
        let channel_stride =
            input_full_shape.iter().skip(1 + patch.input_shape.c_axis()).product::<usize>();
        let rpatch = &patch;
        let data_offsets: Vec<isize> = ndarray::indices(&*patch.output_spatial_shape)
            .into_iter()
            .map(move |coords| {
                coords
                    .slice()
                    .iter()
                    .enumerate()
                    .map(|(ix, x)| x * rpatch.kernel_strides[ix] * input_spatial_dims_strides[ix])
                    .sum::<usize>() as isize
            })
            .collect();
        let kernel_offsets: Vec<isize> = (0..self.input_channels())
            .flat_map(|ici| {
                rpatch
                    .standard_layout_data_field
                    .iter()
                    .map(move |x| x + (ici * channel_stride) as isize)
            })
            .collect();
        let conv =
            (tract_linalg::ops().sconv)(self.output_channels(), kernel_offsets, data_offsets);

        let kernel = self.kernel_reshaped()?;
        let mut packed = unsafe {
            Tensor::uninitialized_aligned::<f32>(&[conv.packed_a_len()], conv.packed_a_alignment())?
        };
        conv.pack_a(
            packed.as_slice_mut()?.as_mut_ptr(),
            kernel.as_slice().unwrap().as_ptr(),
            kernel.strides()[0],
            kernel.strides()[1],
        );

        Ok(super::Direct::new(
            conv,
            input_full_shape.into(),
            patch.output_full_shape(self.output_channels()),
            packed,
        ))
    }

    fn kernel_reshaped<T: Datum>(&self) -> TractResult<Array2<T>> {
        let kernel = self.kernel.to_array_view::<T>()?;
        let kernel_reshaped = (self.output_channels(), kernel.len() / self.output_channels());
        let k = match self.kernel_fmt {
            KernelFormat::HWIO => {
                let mut permutation: Vec<usize> = vec![kernel.ndim() - 1, kernel.ndim() - 2];
                permutation.extend(0..(kernel.ndim() - 2));
                let permuted = kernel.permuted_axes(permutation);
                Array2::<T>::from_shape_vec(
                    kernel_reshaped,
                    permuted.iter().cloned().collect::<Vec<_>>(),
                )?
            }
            KernelFormat::OIHW => kernel.into_shape(kernel_reshaped)?.to_owned(),
        };
        Ok(k)
    }

    fn to_im2col_pair<T>(
        &self,
        input_full_shape: &[usize],
    ) -> TractResult<(Im2Col<T>, TVec<usize>, ConvGemm<T>)>
    where
        T: Datum + Clone + ndarray::LinalgScalar + std::ops::AddAssign<T> + PartialEq,
    {
        let patch = self.patch(input_full_shape);
        let shape: TVec<usize> = patch.output_full_shape(self.output_channels());
        let kernel = self.kernel.to_array_view::<T>()?;

        let m = self.output_channels() / self.group;
        let k = kernel.len() / self.output_channels();
        let n = patch.output_spatial_shape.iter().cloned().product::<usize>();

        let mm: Arc<MatMul<T>> = T::packed_mat_mul(m, k, n)
            .ok_or_else(|| {
                format!(
                    "Can not perfom convolution on {:?} (not a linear algebra type)",
                    T::datum_type()
                )
            })?
            .into();

        let packed_b_len = mm.packed_b_len();

        trace!("Gemm iters={} m={} k={} n={}", patch.input_shape.n_dim() * self.group, m, k, n);

        let kernel = self.kernel_reshaped()?;

        let mut packed_kernels: Vec<Tensor> = vec![];
        let ci_per_group = patch.input_shape.c_dim() / self.group;
        let co_per_group = self.output_channels() / self.group;
        for g in 0..self.group {
            let subkernel =
                kernel.slice_axis(Axis(0), (co_per_group * g..co_per_group * (g + 1)).into());
            let mut packed = unsafe {
                Tensor::uninitialized_aligned::<T>(&[mm.packed_a_len()], mm.packed_a_alignment())?
            };
            mm.pack_a(
                packed.as_slice_mut()?.as_mut_ptr(),
                subkernel.as_ptr(),
                subkernel.strides()[0],
                subkernel.strides()[1],
            );
            packed_kernels.push(packed);
        }

        let bias: Option<ArrayD<T>> = self
            .bias
            .as_ref()
            .map(|bias| -> TractResult<_> {
                let mut bias_shape: Vec<usize> = ::std::iter::repeat(1).take(shape.len()).collect();
                bias_shape[1] = self.output_channels();
                Ok(bias.to_array_view::<T>()?.into_shape(&*bias_shape)?.to_owned())
            })
            .inside_out()?;

        let im2col =
            Im2Col::new(patch.clone(), m, k, n, self.group, ci_per_group, packed_b_len, mm.clone());
        let intermediary_shape = im2col.output_shape()?;
        trace!("im2col: {:?}", im2col);
        let conv_gemm = ConvGemm::new(
            patch,
            shape,
            m,
            k,
            n,
            self.kernel_fmt,
            packed_kernels,
            bias,
            self.group,
            mm.clone(),
        );
        trace!("cvgemm: {:?}", conv_gemm);

        Ok((im2col, intermediary_shape, conv_gemm))
    }

    pub fn to_boxed_im2col_pair<T>(
        &self,
        input_full_shape: &[usize],
    ) -> TractResult<(Box<Op>, TVec<usize>, Box<Op>)>
    where
        T: Datum + Clone + ::ndarray::LinalgScalar + ::std::ops::AddAssign<T> + PartialEq,
    {
        let (op1, shape, op2) = self.to_im2col_pair::<T>(input_full_shape)?;
        Ok((Box::new(op1), shape, Box::new(op2)))
    }

    fn eval_t<T>(&self, mut inputs: TVec<SharedTensor>) -> TractResult<TVec<SharedTensor>>
    where
        T: Datum + Clone + ::ndarray::LinalgScalar + ::std::ops::AddAssign<T> + PartialEq,
    {
        let input = args_1!(inputs);
        let (im2col, _shape, conv_gemm) = self.to_im2col_pair::<T>(input.shape())?;
        let mega = im2col.im2col(&input.to_array_view()?)?;
        let output = conv_gemm.conv_gemm(&mega.to_array_view::<T>()?.into_dimensionality()?)?;
        Ok(tvec!(output.into()))
    }

    pub fn rm_dummy_axis(&self, axis: usize) -> TractResult<Option<ConvUnary>> {
        let shape = self.data_fmt.shape(&self.full_input_shape);
        if axis < shape.h_axis() {
            return Ok(None);
        }
        let geo_axis = axis - shape.h_axis();
        if geo_axis >= shape.hw_rank() {
            return Ok(None);
        }
        if self.dilations[geo_axis] != 1
            || self.strides[geo_axis] != 1
            || !self.padding.valid_dim(geo_axis)
        {
            return Ok(None);
        }
        let kernel_spatial_shape =
            &self.kernel.shape()[self.kernel_fmt.h_axis()..][..shape.hw_rank()];
        if kernel_spatial_shape[geo_axis] != 1 {
            return Ok(None);
        }
        fn copy_rm_nth<D: DimLike>(input: &[D], nth: usize) -> TVec<D> {
            input.iter().enumerate().filter(|&(ax, _)| ax != nth).map(|(_, &d)| d).collect()
        }
        let kernel_shape: TVec<usize> =
            copy_rm_nth(self.kernel.shape().clone(), geo_axis + self.kernel_fmt.h_axis());
        let kernel = self.kernel.clone().into_shape(&kernel_shape)?;
        let new_op = ConvUnary {
            data_fmt: self.data_fmt,
            kernel_fmt: self.kernel_fmt,
            padding: self.padding.rm_axis(geo_axis),
            dilations: copy_rm_nth(&self.dilations, geo_axis),
            strides: copy_rm_nth(&self.strides, geo_axis),
            kernel,
            bias: self.bias.clone(),
            full_input_shape: copy_rm_nth(&self.full_input_shape, axis),
            full_output_shape: copy_rm_nth(&self.full_output_shape, axis),
            group: self.group,
        };
        Ok(Some(new_op))
    }
}

impl Op for ConvUnary {
    fn name(&self) -> Cow<str> {
        "ConvUnary".into()
    }

    fn declutter(
        &self,
        model: &TypedModel,
        node: &TypedNode,
    ) -> TractResult<Option<TypedModelPatch>> {
        use crate::ops::array::{AddDims, RmDims};
        if let (Some(add_node), Some(rm_node)) = (model.single_prec(node.id)?, model.single_succ(node.id)?) {
            if let (Some(add_op), Some(rm_op)) = (add_node.op_as::<AddDims>(), rm_node.op_as::<RmDims>()) {
                if add_op.axes.len() == 1 && rm_op.axes == add_op.axes {
                    let axis = add_op.axes[0];
                    if let Some(op) = self.rm_dummy_axis(axis)? {
                        let mut patch = TypedModelPatch::default();
                        patch.tap_model(&model, model.single_prec(node.id)?.unwrap().inputs[0])?;
                        let out = patch.model.chain(&*node.name, op, tvec!(rm_node.outputs[0].fact.clone()))?;
                        patch.shunt_outside(OutletId::new(rm_node.id, 0), OutletId::new(out, 0))?;
                        return Ok(Some(patch))
                    }
                }
            }
        }
        Ok(None)
    }

    fn codegen(
        &self,
        model: &TypedModel,
        node: &TypedNode,
    ) -> TractResult<Option<TypedModelPatch>> {
        let inputs = model.node_input_facts(node.id)?;
        let spatial_rank = self.full_input_shape.len() - 2;
        let kernel_spatial_shape = &self.kernel.shape()[self.kernel_fmt.h_axis()..][..spatial_rank];
        if kernel_spatial_shape.iter().product::<usize>() == 1
            && self.dilations.iter().all(|&x| x == 1)
            && self.strides.iter().all(|&x| x == 1)
            && self.group == 1
            && self.bias.is_none()
            && (0..spatial_rank).all(|ax| self.padding.valid_dim(ax))
        {
            if self.kernel_fmt == KernelFormat::HWIO && self.data_fmt == DataFormat::NHWC {
                use crate::ops::math::mat_mul::MatMulUnaryA;
                let kernel_shape = &self.kernel.shape()[spatial_rank..];
                let kernel = self.kernel.clone().into_shape(&kernel_shape)?;
                return Ok(Some(TypedModelPatch::single_unary_op(
                    model,
                    node,
                    MatMulUnaryA::new(kernel),
                )?));
            }
        } else {
            if let Some(shape) = inputs[0].shape.as_finite() {
                let dt = inputs[0].datum_type;
                if (0..spatial_rank).all(|ax| self.padding.valid_dim(ax))
                    && dt == f32::datum_type()
                    && self.group == 1
                    && self.bias.is_none()
                {
                    let op = self.to_direct(&*shape)?;
                    return Ok(Some(TypedModelPatch::single_unary_op(model, node, op)?));
                } else {
                    let (op1, shape, op2) =
                        dispatch_floatlike!(Self::to_boxed_im2col_pair(dt)(self, &shape))?;
                    let mut patch = TypedModelPatch::default();
                    let _ = patch.tap_model(&model, node.inputs[0])?;
                    patch.chain(
                        format!("{}-im2col", node.name),
                        op1,
                        tvec!(TypedTensorInfo {
                            shape: ShapeInfo::from(&*shape),
                            datum_type: dt,
                            konst: None,
                        }),
                    )?;
                    let mm = patch.chain(
                        format!("{}-convmm", node.name),
                        op2,
                        tvec!(node.outputs[0].fact.clone()),
                    )?;
                    patch.shunt_outside(OutletId::new(node.id, 0), OutletId::new(mm, 0))?;
                    return Ok(Some(patch));
                }
            }
        }
        Ok(None)
    }

    fn pulsify(
        &self,
        _source: &NormalizedModel,
        node: &NormalizedNode,
        target: &mut PulsedModel,
        mapping: &HashMap<OutletId, OutletId>,
    ) -> TractResult<TVec<OutletId>> {
        let input = mapping[&node.inputs[0]];
        let mut fact = target.fact(input)?.clone();
        let shape = self.data_fmt.shape(&fact.shape);
        if fact.axis == shape.n_axis() {
            let mut op = self.clone();
            op.full_output_shape[fact.axis] = fact.pulse().to_dim();
            fact.shape = op
                .full_output_shape
                .iter()
                .enumerate()
                .map(|(ax, &d)| {
                    if ax == fact.axis {
                        fact.pulse()
                    } else {
                        d.to_integer().unwrap() as usize
                    }
                })
                .collect();
            let id = target.chain_after(input, &*node.name, self.clone(), tvec!(fact))?;
            Ok(tvec!(OutletId::new(id, 0)))
        } else if fact.axis == shape.c_axis() {
            bail!("Can not pulsify convolution alongs the input channel axis");
        } else {
            let spatial_rank = self.full_input_shape.len() - 2;
            let geo_axis = fact.axis - shape.h_axis();
            let kernel_spatial_shape =
                &self.kernel.shape()[self.kernel_fmt.h_axis()..][..spatial_rank];
            let kernel_len = (kernel_spatial_shape[geo_axis] - 1)
                * self.strides[geo_axis] // TODO do we really need * strides here ?
                * self.dilations[geo_axis];
            let mut augmented_fact = fact.clone();
            augmented_fact.shape[augmented_fact.axis] += kernel_len;
            augmented_fact.delay += kernel_len;

            let mut conv_op = self.clone();
            conv_op.full_input_shape[fact.axis] = augmented_fact.pulse().to_dim();
            conv_op.full_output_shape[fact.axis] =
                (augmented_fact.pulse() - kernel_len / self.strides[geo_axis]).to_dim();
            let mut conv_fact = fact.clone();
            conv_fact.shape = self
                .full_output_shape
                .iter()
                .enumerate()
                .map(|(ax, &d)| {
                    if ax == fact.axis {
                        fact.pulse() / self.strides[geo_axis]
                    } else {
                        d.to_integer().unwrap() as usize
                    }
                })
                .collect();
            conv_fact.delay += kernel_len;
            conv_fact.dim -= kernel_len.to_dim();

            let delay = crate::pulse::delay::Delay::new(fact, 0, kernel_len);
            target.chain_after(input,format!("{}/Delay", node.name), delay, tvec!(augmented_fact))?;
            let id = target.chain(&*node.name, conv_op, tvec!(conv_fact))?;

            Ok(tvec!(OutletId::new(id, 0)))
        }
    }
}

impl StatelessOp for ConvUnary {
    fn eval(&self, inputs: TVec<SharedTensor>) -> TractResult<TVec<SharedTensor>> {
        dispatch_floatlike!(Self::eval_t(inputs[0].datum_type())(self, inputs))
    }
}

impl InferenceRulesOp for ConvUnary {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        s: &mut Solver<'r>,
        inputs: &'p [TensorProxy],
        outputs: &'p [TensorProxy],
    ) -> InferenceResult {
        check_input_arity(&inputs, 1)?;
        check_output_arity(&outputs, 1)?;
        s.equals(&inputs[0].datum_type, &outputs[0].datum_type)?;
        s.equals(&inputs[0].shape, self.full_input_shape.clone())?;
        s.equals(&outputs[0].shape, self.full_output_shape.clone())?;
        Ok(())
    }
}
