use core::any::{Any, TypeId};

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bytemuck::AnyBitPattern;
use half::{bf16, f16};

use crate::{
    quantization::{AffineQuantization, Quantization, QuantizationStrategy},
    tensor::Shape,
    DType, Distribution, Element, ElementConversion,
};

use num_traits::pow::Pow;

#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use num_traits::Float;

use rand::RngCore;

use super::quantization::{QParams, QuantizationScheme, QuantizationType, SymmetricQuantization};

/// The things that can go wrong when manipulating tensor data.
#[derive(Debug)]
pub enum DataError {
    /// Failed to cast the values to a specified element type.
    CastError(bytemuck::checked::CheckedCastError),
    /// Invalid target element type.
    TypeMismatch(String),
}

/// Data structure for tensors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TensorData {
    /// The values of the tensor (as bytes).
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,

    /// The shape of the tensor.
    pub shape: Vec<usize>,

    /// The data type of the tensor.
    pub dtype: DType,
}

fn value_into_bytes<E>(mut value: Vec<E>) -> Vec<u8> {
    // Ensure `E` satisfies the `Pod` trait requirements
    assert_eq!(core::mem::size_of::<E>() % core::mem::size_of::<u8>(), 0);

    let factor = core::mem::size_of::<E>() / core::mem::size_of::<u8>();
    let len = value.len() * factor;
    let capacity = value.capacity() * factor;
    let ptr = value.as_mut_ptr();

    core::mem::forget(value);

    unsafe { Vec::from_raw_parts(ptr as *mut u8, len, capacity) }
}

impl TensorData {
    /// Creates a new tensor data structure.
    pub fn new<E: Element, S: Into<Vec<usize>>>(mut value: Vec<E>, shape: S) -> Self {
        let shape = shape.into();
        Self::validate_data_shape(&mut value, &shape);
        Self::init(value, shape, E::dtype())
    }

    fn validate_data_shape<E>(value: &mut Vec<E>, shape: &[usize]) {
        // Ensure shape is valid
        let shape_numel = Self::numel(shape);
        value.truncate(shape_numel);
        let numel = value.len();
        assert_eq!(
            shape_numel, numel,
            "Shape {:?} is invalid for input of size {:?}",
            shape, numel,
        );
    }

    /// Creates a new quantized tensor data structure.
    pub fn quantized<E: Element, S: Into<Vec<usize>>>(
        mut value: Vec<E>,
        shape: S,
        strategy: QuantizationStrategy,
    ) -> Self {
        let shape = shape.into();
        Self::validate_data_shape(&mut value, &shape);

        let mut value = value_into_bytes(value);

        // Quantization parameters are packed at the end of the tensor data.
        // As such, the last bytes always correspond to the scale parameter.
        // If the quantization scheme includes an offset (zero-point) parameter, it is next to last.
        match strategy {
            QuantizationStrategy::PerTensorAffineInt8(q) => {
                let scale_bytes = bytemuck::bytes_of(&q.scale);
                let offset_bytes = bytemuck::bytes_of(&q.offset);
                value.extend_from_slice(offset_bytes);
                value.extend_from_slice(scale_bytes);
            }
            QuantizationStrategy::PerTensorSymmetricInt8(q) => {
                let scale_bytes = bytemuck::bytes_of(&q.scale);
                value.extend_from_slice(scale_bytes);
            }
        }

        Self::init(value, shape, DType::QFloat(strategy.scheme()))
    }

    /// Initializes a new tensor data structure from the provided values.
    fn init<E: Element, S: Into<Vec<usize>>>(value: Vec<E>, shape: S, dtype: DType) -> Self {
        Self {
            bytes: value_into_bytes(value),
            shape: shape.into(),
            dtype,
        }
    }

    fn try_as_slice<E: Element>(&self) -> Result<&[E], DataError> {
        bytemuck::checked::try_cast_slice(&self.bytes).map_err(DataError::CastError)
    }

    /// Returns the immutable slice view of the tensor data.
    pub fn as_slice<E: Element>(&self) -> Result<&[E], DataError> {
        if E::dtype() == self.dtype {
            self.try_as_slice()
        } else {
            Err(DataError::TypeMismatch(format!(
                "Invalid target element type (expected {:?}, got {:?})",
                self.dtype,
                E::dtype()
            )))
        }
    }

    /// Returns the mutable slice view of the tensor data.
    ///
    /// # Panics
    /// If the target element type is different from the stored element type.
    pub fn as_mut_slice<E: Element>(&mut self) -> Result<&mut [E], DataError> {
        if E::dtype() == self.dtype {
            bytemuck::checked::try_cast_slice_mut(&mut self.bytes).map_err(DataError::CastError)
        } else {
            Err(DataError::TypeMismatch(format!(
                "Invalid target element type (expected {:?}, got {:?})",
                self.dtype,
                E::dtype()
            )))
        }
    }

    /// Returns the tensor data as a vector of scalar values.
    pub fn to_vec<E: Element>(&self) -> Result<Vec<E>, DataError> {
        Ok(self.as_slice()?.to_vec())
    }

    /// Returns the tensor data as a vector of scalar values.
    pub fn into_vec<E: Element>(mut self) -> Result<Vec<E>, DataError> {
        if E::dtype() != self.dtype {
            return Err(DataError::TypeMismatch(format!(
                "Invalid target element type (expected {:?}, got {:?})",
                self.dtype,
                E::dtype()
            )));
        }

        let capacity_bytes = self.bytes.capacity();
        let length_bytes = self.bytes.len();
        let size_elem = core::mem::size_of::<E>();

        let capacity = capacity_bytes / size_elem;
        let length = length_bytes / size_elem;

        unsafe {
            let ptr = self.bytes.as_mut_ptr();
            core::mem::forget(self.bytes);

            Ok(Vec::from_raw_parts(ptr.cast::<E>(), length, capacity))
        }
    }

    /// Returns an iterator over the values of the tensor data.
    pub fn iter<E: Element>(&self) -> Box<dyn Iterator<Item = E> + '_> {
        if E::dtype() == self.dtype {
            Box::new(bytemuck::checked::cast_slice(&self.bytes).iter().copied())
        } else {
            match self.dtype {
                DType::I8 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &i8| e.elem::<E>()),
                ),
                DType::I16 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &i16| e.elem::<E>()),
                ),
                DType::I32 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &i32| e.elem::<E>()),
                ),
                DType::I64 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &i64| e.elem::<E>()),
                ),
                DType::U8 => Box::new(self.bytes.iter().map(|e| e.elem::<E>())),
                DType::U32 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &u32| e.elem::<E>()),
                ),
                DType::U64 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &u64| e.elem::<E>()),
                ),
                DType::BF16 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &bf16| e.elem::<E>()),
                ),
                DType::F16 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &f16| e.elem::<E>()),
                ),
                DType::F32 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &f32| e.elem::<E>()),
                ),
                DType::F64 => Box::new(
                    bytemuck::checked::cast_slice(&self.bytes)
                        .iter()
                        .map(|e: &f64| e.elem::<E>()),
                ),
                // bool is a byte value equal to either 0 or 1
                DType::Bool => Box::new(self.bytes.iter().map(|e| e.elem::<E>())),
                DType::QFloat(scheme) => match scheme {
                    // NOTE: the tensor values are not dequantized, simply cast to their quantized type
                    QuantizationScheme::PerTensorAffine(QuantizationType::QInt8) => Box::new(
                        bytemuck::checked::cast_slice(self.tensor_bytes())
                            .iter()
                            .map(|e: &i8| e.elem::<E>()),
                    ),

                    QuantizationScheme::PerTensorSymmetric(QuantizationType::QInt8) => Box::new(
                        bytemuck::checked::cast_slice(self.tensor_bytes())
                            .iter()
                            .map(|e: &i8| e.elem::<E>()),
                    ),
                },
            }
        }
    }

    /// Returns the total number of elements of the tensor data.
    pub fn num_elements(&self) -> usize {
        Self::numel(&self.shape)
    }

    fn numel(shape: &[usize]) -> usize {
        shape.iter().product()
    }

    /// Populates the data with random values.
    pub fn random<E: Element, R: RngCore, S: Into<Vec<usize>>>(
        shape: S,
        distribution: Distribution,
        rng: &mut R,
    ) -> Self {
        let shape = shape.into();
        let num_elements = Self::numel(&shape);
        let mut data = Vec::with_capacity(num_elements);

        for _ in 0..num_elements {
            data.push(E::random(distribution, rng));
        }

        TensorData::new(data, shape)
    }

    /// Populates the data with zeros.
    pub fn zeros<E: Element, S: Into<Vec<usize>>>(shape: S) -> TensorData {
        let shape = shape.into();
        let num_elements = Self::numel(&shape);
        let mut data = Vec::<E>::with_capacity(num_elements);

        for _ in 0..num_elements {
            data.push(0.elem());
        }

        TensorData::new(data, shape)
    }

    /// Populates the data with ones.
    pub fn ones<E: Element, S: Into<Vec<usize>>>(shape: S) -> TensorData {
        let shape = shape.into();
        let num_elements = Self::numel(&shape);
        let mut data = Vec::<E>::with_capacity(num_elements);

        for _ in 0..num_elements {
            data.push(1.elem());
        }

        TensorData::new(data, shape)
    }

    /// Populates the data with the given value
    pub fn full<E: Element, S: Into<Vec<usize>>>(shape: S, fill_value: E) -> TensorData {
        let shape = shape.into();
        let num_elements = Self::numel(&shape);
        let mut data = Vec::<E>::with_capacity(num_elements);
        for _ in 0..num_elements {
            data.push(fill_value)
        }

        TensorData::new(data, shape)
    }

    /// Converts the data to a different element type.
    pub fn convert<E: Element>(self) -> Self {
        if E::dtype() == self.dtype {
            self
        } else if core::mem::size_of::<E>() == self.dtype.size()
            && !matches!(self.dtype, DType::Bool | DType::QFloat(_))
        {
            match self.dtype {
                DType::F64 => self.convert_inplace::<f64, E>(),
                DType::F32 => self.convert_inplace::<f32, E>(),
                DType::F16 => self.convert_inplace::<f16, E>(),
                DType::BF16 => self.convert_inplace::<bf16, E>(),
                DType::I64 => self.convert_inplace::<i64, E>(),
                DType::I32 => self.convert_inplace::<i32, E>(),
                DType::I16 => self.convert_inplace::<i16, E>(),
                DType::I8 => self.convert_inplace::<i8, E>(),
                DType::U64 => self.convert_inplace::<u64, E>(),
                DType::U32 => self.convert_inplace::<u32, E>(),
                DType::U8 => self.convert_inplace::<u8, E>(),
                DType::Bool | DType::QFloat(_) => unreachable!(),
            }
        } else {
            TensorData::new(self.iter::<E>().collect(), self.shape)
        }
    }

    fn convert_inplace<Current: Element + AnyBitPattern, Target: Element>(mut self) -> Self {
        let step = core::mem::size_of::<Current>();

        for offset in 0..(self.bytes.len() / step) {
            let start = offset * step;
            let end = start + step;

            let slice_old = &mut self.bytes[start..end];
            let val: Current = *bytemuck::from_bytes(slice_old);
            let val = &val.elem::<Target>();
            let slice_new = bytemuck::bytes_of(val);

            slice_old.clone_from_slice(slice_new);
        }
        self.dtype = Target::dtype();

        self
    }

    /// Returns the data as a slice of bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Applies the data quantization strategy.
    ///
    /// # Panics
    ///
    /// Panics if the data type is not supported for quantization.
    pub fn with_quantization(self, quantization: QuantizationStrategy) -> Self {
        assert_eq!(
            self.dtype,
            DType::F32,
            "Only f32 data type can be quantized"
        );
        match &quantization {
            QuantizationStrategy::PerTensorAffineInt8(strategy) => TensorData::quantized(
                strategy.quantize(self.as_slice().unwrap()),
                self.shape,
                quantization,
            ),
            QuantizationStrategy::PerTensorSymmetricInt8(strategy) => TensorData::quantized(
                strategy.quantize(self.as_slice().unwrap()),
                self.shape,
                quantization,
            ),
        }
    }

    /// Returns the values of the tensor as bytes.
    ///
    /// Takes into account the quantization parameters to ignore since they are packed
    /// into the tensor data bytes.
    fn tensor_bytes(&self) -> &[u8] {
        match self.dtype {
            DType::QFloat(scheme) => {
                let scale_size = core::mem::size_of::<f32>();
                let mut tensor_bytes_end = self.bytes.len() - scale_size;

                if let QuantizationScheme::PerTensorAffine(QuantizationType::QInt8) = scheme {
                    tensor_bytes_end -= core::mem::size_of::<i8>();
                }

                &self.bytes[..tensor_bytes_end]
            }
            _ => self.bytes.as_slice(),
        }
    }

    /// Get the quantization parameters for a quantized data type.
    pub fn get_q_params<E: Element, Q: Element>(&self) -> Option<QParams<E, Q>> {
        if let DType::QFloat(scheme) = &self.dtype {
            let total_bytes = self.bytes.len();

            // Quantization parameters are packed at the end of the tensor data.
            // As such, the last bytes always correspond to the scale parameter.
            // If the quantization scheme includes an offset (zero-point) parameter, it is next to last.
            let scale_size = core::mem::size_of::<E>();
            let scale_bytes = &self.bytes[total_bytes - scale_size..];
            // The starting memory address isn't guaranteed to be  divisible by the target type's
            // alignment size, so using `from_bytes` could fail. Instead, we use `read_unaligned`.
            let scale = bytemuck::checked::pod_read_unaligned(scale_bytes);
            let mut offset = None;

            if let QuantizationScheme::PerTensorAffine(_) = scheme {
                let offset_size = core::mem::size_of::<Q>();
                let offset_bytes =
                    &self.bytes[total_bytes - scale_size - offset_size..total_bytes - scale_size];
                offset = Some(bytemuck::checked::pod_read_unaligned(offset_bytes))
            }

            Some(QParams { scale, offset })
        } else {
            None
        }
    }

    /// Dequantizes the data according to its quantization scheme.
    pub fn dequantize(self) -> Result<Self, DataError> {
        if let DType::QFloat(scheme) = &self.dtype {
            let qparams = self.get_q_params::<f32, i8>().unwrap();
            match scheme {
                QuantizationScheme::PerTensorAffine(QuantizationType::QInt8) => {
                    let strategy = AffineQuantization::<f32, i8, i32>::init(
                        qparams.scale,
                        qparams.offset.unwrap(),
                    );
                    let value =
                        strategy.dequantize(bytemuck::checked::cast_slice(self.tensor_bytes()));
                    Ok(Self::new(value, self.shape))
                }
                QuantizationScheme::PerTensorSymmetric(QuantizationType::QInt8) => {
                    let strategy = SymmetricQuantization::<f32, i8>::init(qparams.scale);
                    let value =
                        strategy.dequantize(bytemuck::checked::cast_slice(self.tensor_bytes()));
                    Ok(Self::new(value, self.shape))
                }
            }
        } else {
            Err(DataError::TypeMismatch(format!(
                "Expected quantized data, got {:?}",
                self.dtype
            )))
        }
    }

    /// Asserts the data is approximately equal to another data.
    ///
    /// # Arguments
    ///
    /// * `other` - The other data.
    /// * `precision` - The precision of the comparison.
    ///
    /// # Panics
    ///
    /// Panics if the data is not approximately equal.
    #[track_caller]
    pub fn assert_approx_eq(&self, other: &Self, precision: usize) {
        let tolerance = 0.1.pow(precision as f64);

        self.assert_approx_eq_diff(other, tolerance)
    }

    /// Asserts the data is equal to another data.
    ///
    /// # Arguments
    ///
    /// * `other` - The other data.
    /// * `strict` - If true, the data types must the be same.
    ///              Otherwise, the comparison is done in the current data type.
    ///
    /// # Panics
    ///
    /// Panics if the data is not equal.
    #[track_caller]
    pub fn assert_eq(&self, other: &Self, strict: bool) {
        if strict {
            assert_eq!(
                self.dtype, other.dtype,
                "Data types differ ({:?} != {:?})",
                self.dtype, other.dtype
            );
        }

        match self.dtype {
            DType::F64 => self.assert_eq_elem::<f64>(other),
            DType::F32 => self.assert_eq_elem::<f32>(other),
            DType::F16 => self.assert_eq_elem::<f16>(other),
            DType::BF16 => self.assert_eq_elem::<bf16>(other),
            DType::I64 => self.assert_eq_elem::<i64>(other),
            DType::I32 => self.assert_eq_elem::<i32>(other),
            DType::I16 => self.assert_eq_elem::<i16>(other),
            DType::I8 => self.assert_eq_elem::<i8>(other),
            DType::U64 => self.assert_eq_elem::<u64>(other),
            DType::U32 => self.assert_eq_elem::<u32>(other),
            DType::U8 => self.assert_eq_elem::<u8>(other),
            DType::Bool => self.assert_eq_elem::<bool>(other),
            DType::QFloat(q) => {
                // Strict or not, it doesn't make sense to compare quantized data to not quantized data for equality
                let q_other = if let DType::QFloat(q_other) = other.dtype {
                    q_other
                } else {
                    panic!("Quantized data differs from other not quantized data")
                };
                match (q, q_other) {
                    (
                        QuantizationScheme::PerTensorAffine(QuantizationType::QInt8),
                        QuantizationScheme::PerTensorAffine(QuantizationType::QInt8),
                    )
                    | (
                        QuantizationScheme::PerTensorSymmetric(QuantizationType::QInt8),
                        QuantizationScheme::PerTensorSymmetric(QuantizationType::QInt8),
                    ) => self.assert_eq_elem::<i8>(other),
                    _ => panic!("Quantization schemes differ ({:?} != {:?})", q, q_other),
                }
            }
        }
    }

    #[track_caller]
    fn assert_eq_elem<E: Element>(&self, other: &Self) {
        let mut message = String::new();
        if self.shape != other.shape {
            message += format!(
                "\n  => Shape is different: {:?} != {:?}",
                self.shape, other.shape
            )
            .as_str();
        }

        let mut num_diff = 0;
        let max_num_diff = 5;
        for (i, (a, b)) in self.iter::<E>().zip(other.iter::<E>()).enumerate() {
            if a.cmp(&b).is_ne() {
                // Only print the first 5 different values.
                if num_diff < max_num_diff {
                    message += format!("\n  => Position {i}: {a} != {b}").as_str();
                }
                num_diff += 1;
            }
        }

        if num_diff >= max_num_diff {
            message += format!("\n{} more errors...", num_diff - max_num_diff).as_str();
        }

        if !message.is_empty() {
            panic!("Tensors are not eq:{}", message);
        }
    }

    /// Asserts the data is approximately equal to another data.
    ///
    /// # Arguments
    ///
    /// * `other` - The other data.
    /// * `tolerance` - The tolerance of the comparison.
    ///
    /// # Panics
    ///
    /// Panics if the data is not approximately equal.
    #[track_caller]
    pub fn assert_approx_eq_diff(&self, other: &Self, tolerance: f64) {
        let mut message = String::new();
        if self.shape != other.shape {
            message += format!(
                "\n  => Shape is different: {:?} != {:?}",
                self.shape, other.shape
            )
            .as_str();
        }

        let iter = self.iter::<f64>().zip(other.iter::<f64>());

        let mut num_diff = 0;
        let max_num_diff = 5;

        for (i, (a, b)) in iter.enumerate() {
            //if they are both nan, then they are equally nan
            let both_nan = a.is_nan() && b.is_nan();
            //this works for both infinities
            let both_inf = a.is_infinite() && b.is_infinite() && ((a > 0.) == (b > 0.));

            if both_nan || both_inf {
                continue;
            }

            let err = ((a - b).pow(2.0f64)).sqrt();

            if err > tolerance || err.is_nan() {
                // Only print the first 5 different values.
                if num_diff < max_num_diff {
                    message += format!(
                        "\n  => Position {i}: {a} != {b} | difference {err} > tolerance \
                         {tolerance}"
                    )
                    .as_str();
                }
                num_diff += 1;
            }
        }

        if num_diff >= max_num_diff {
            message += format!("\n{} more errors...", num_diff - 5).as_str();
        }

        if !message.is_empty() {
            panic!("Tensors are not approx eq:{}", message);
        }
    }

    /// Asserts each value is within a given range.
    ///
    /// # Arguments
    ///
    /// * `range` - The range.
    ///
    /// # Panics
    ///
    /// If any value is not within the half-open range bounded inclusively below
    /// and exclusively above (`start..end`).
    pub fn assert_within_range<E: Element>(&self, range: core::ops::Range<E>) {
        let start = range.start.elem::<f32>();
        let end = range.end.elem::<f32>();

        for elem in self.iter::<f32>() {
            if elem < start || elem >= end {
                panic!("Element ({elem:?}) is not within range {range:?}");
            }
        }
    }
}

impl<E: Element, const A: usize> From<[E; A]> for TensorData {
    fn from(elems: [E; A]) -> Self {
        TensorData::new(elems.to_vec(), [A])
    }
}

impl<const A: usize> From<[usize; A]> for TensorData {
    fn from(elems: [usize; A]) -> Self {
        TensorData::new(elems.iter().map(|&e| e as i64).collect(), [A])
    }
}

impl From<&[usize]> for TensorData {
    fn from(elems: &[usize]) -> Self {
        let mut data = Vec::with_capacity(elems.len());
        for elem in elems.iter() {
            data.push(*elem as i64);
        }

        TensorData::new(data, [elems.len()])
    }
}

impl<E: Element> From<&[E]> for TensorData {
    fn from(elems: &[E]) -> Self {
        let mut data = Vec::with_capacity(elems.len());
        for elem in elems.iter() {
            data.push(*elem);
        }

        TensorData::new(data, [elems.len()])
    }
}

impl<E: Element, const A: usize, const B: usize> From<[[E; B]; A]> for TensorData {
    fn from(elems: [[E; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B);
        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                data.push(elem);
            }
        }

        TensorData::new(data, [A, B])
    }
}

impl<E: Element, const A: usize, const B: usize, const C: usize> From<[[[E; C]; B]; A]>
    for TensorData
{
    fn from(elems: [[[E; C]; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B * C);

        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                for elem in elem.into_iter().take(C) {
                    data.push(elem);
                }
            }
        }

        TensorData::new(data, [A, B, C])
    }
}

impl<E: Element, const A: usize, const B: usize, const C: usize, const D: usize>
    From<[[[[E; D]; C]; B]; A]> for TensorData
{
    fn from(elems: [[[[E; D]; C]; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B * C * D);

        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                for elem in elem.into_iter().take(C) {
                    for elem in elem.into_iter().take(D) {
                        data.push(elem);
                    }
                }
            }
        }

        TensorData::new(data, [A, B, C, D])
    }
}

impl<
        Elem: Element,
        const A: usize,
        const B: usize,
        const C: usize,
        const D: usize,
        const E: usize,
    > From<[[[[[Elem; E]; D]; C]; B]; A]> for TensorData
{
    fn from(elems: [[[[[Elem; E]; D]; C]; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B * C * D * E);

        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                for elem in elem.into_iter().take(C) {
                    for elem in elem.into_iter().take(D) {
                        for elem in elem.into_iter().take(E) {
                            data.push(elem);
                        }
                    }
                }
            }
        }

        TensorData::new(data, [A, B, C, D, E])
    }
}

impl core::fmt::Display for TensorData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let fmt = match self.dtype {
            DType::F64 => format!("{:?}", self.as_slice::<f64>().unwrap()),
            DType::F32 => format!("{:?}", self.as_slice::<f32>().unwrap()),
            DType::F16 => format!("{:?}", self.as_slice::<f16>().unwrap()),
            DType::BF16 => format!("{:?}", self.as_slice::<bf16>().unwrap()),
            DType::I64 => format!("{:?}", self.as_slice::<i64>().unwrap()),
            DType::I32 => format!("{:?}", self.as_slice::<i32>().unwrap()),
            DType::I16 => format!("{:?}", self.as_slice::<i16>().unwrap()),
            DType::I8 => format!("{:?}", self.as_slice::<i8>().unwrap()),
            DType::U64 => format!("{:?}", self.as_slice::<u64>().unwrap()),
            DType::U32 => format!("{:?}", self.as_slice::<u32>().unwrap()),
            DType::U8 => format!("{:?}", self.as_slice::<u8>().unwrap()),
            DType::Bool => format!("{:?}", self.as_slice::<bool>().unwrap()),
            DType::QFloat(scheme) => match scheme {
                QuantizationScheme::PerTensorAffine(QuantizationType::QInt8)
                | QuantizationScheme::PerTensorSymmetric(QuantizationType::QInt8) => {
                    format!("{:?} {scheme:?}", self.try_as_slice::<i8>().unwrap())
                }
            },
        };
        f.write_str(fmt.as_str())
    }
}

/// Data structure for serializing and deserializing tensor data.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq, Clone, new)]
#[deprecated(
    since = "0.14.0",
    note = "the internal data format has changed, please use `TensorData` instead"
)]
pub struct DataSerialize<E> {
    /// The values of the tensor.
    pub value: Vec<E>,
    /// The shape of the tensor.
    pub shape: Vec<usize>,
}

/// Data structure for tensors.
#[derive(new, Debug, Clone, PartialEq, Eq)]
#[deprecated(
    since = "0.14.0",
    note = "the internal data format has changed, please use `TensorData` instead"
)]
pub struct Data<E, const D: usize> {
    /// The values of the tensor.
    pub value: Vec<E>,

    /// The shape of the tensor.
    pub shape: Shape,
}

#[allow(deprecated)]
impl<const D: usize, E: Element> Data<E, D> {
    /// Converts the data to a different element type.
    pub fn convert<EOther: Element>(self) -> Data<EOther, D> {
        let value: Vec<EOther> = self.value.into_iter().map(|a| a.elem()).collect();

        Data {
            value,
            shape: self.shape,
        }
    }

    /// Asserts each value is within a given range.
    ///
    /// # Arguments
    ///
    /// * `range` - The range.
    ///
    /// # Panics
    ///
    /// If any value is not within the half-open range bounded inclusively below
    /// and exclusively above (`start..end`).
    pub fn assert_within_range<EOther: Element>(&self, range: core::ops::Range<EOther>) {
        let start = range.start.elem::<f32>();
        let end = range.end.elem::<f32>();

        for elem in self.value.iter() {
            let elem = elem.elem::<f32>();
            if elem < start || elem >= end {
                panic!("Element ({elem:?}) is not within range {range:?}");
            }
        }
    }
}

#[allow(deprecated)]
impl<E: Element> DataSerialize<E> {
    /// Converts the data to a different element type.
    pub fn convert<EOther: Element>(self) -> DataSerialize<EOther> {
        if TypeId::of::<E>() == TypeId::of::<EOther>() {
            let cast: Box<dyn Any> = Box::new(self);
            let cast: Box<DataSerialize<EOther>> = cast.downcast().unwrap();
            return *cast;
        }

        let value: Vec<EOther> = self.value.into_iter().map(|a| a.elem()).collect();

        DataSerialize {
            value,
            shape: self.shape,
        }
    }

    /// Converts the data to the new [TensorData] format.
    pub fn into_tensor_data(self) -> TensorData {
        TensorData::new(self.value, self.shape)
    }
}

#[allow(deprecated)]
impl<E: Element, const D: usize> Data<E, D> {
    /// Populates the data with random values.
    pub fn random<R: RngCore>(shape: Shape, distribution: Distribution, rng: &mut R) -> Self {
        let num_elements = shape.num_elements();
        let mut data = Vec::with_capacity(num_elements);

        for _ in 0..num_elements {
            data.push(E::random(distribution, rng));
        }

        Data::new(data, shape)
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug, const D: usize> Data<E, D>
where
    E: Element,
{
    /// Populates the data with zeros.
    pub fn zeros<S: Into<Shape>>(shape: S) -> Data<E, D> {
        let shape = shape.into();
        let num_elements = shape.num_elements();
        let mut data = Vec::with_capacity(num_elements);

        for _ in 0..num_elements {
            data.push(0.elem());
        }

        Data::new(data, shape)
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug, const D: usize> Data<E, D>
where
    E: Element,
{
    /// Populates the data with ones.
    pub fn ones(shape: Shape) -> Data<E, D> {
        let num_elements = shape.num_elements();
        let mut data = Vec::with_capacity(num_elements);

        for _ in 0..num_elements {
            data.push(1.elem());
        }

        Data::new(data, shape)
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug, const D: usize> Data<E, D>
where
    E: Element,
{
    /// Populates the data with the given value
    pub fn full(shape: Shape, fill_value: E) -> Data<E, D> {
        let num_elements = shape.num_elements();
        let mut data = Vec::with_capacity(num_elements);
        for _ in 0..num_elements {
            data.push(fill_value)
        }

        Data::new(data, shape)
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug + Copy, const D: usize> Data<E, D> {
    /// Serializes the data.
    ///
    /// # Returns
    ///
    /// The serialized data.
    pub fn serialize(&self) -> DataSerialize<E> {
        DataSerialize {
            value: self.value.clone(),
            shape: self.shape.dims.to_vec(),
        }
    }
}

#[allow(deprecated)]
impl<E: Into<f64> + Clone + core::fmt::Debug + PartialEq, const D: usize> Data<E, D> {
    /// Asserts the data is approximately equal to another data.
    ///
    /// # Arguments
    ///
    /// * `other` - The other data.
    /// * `precision` - The precision of the comparison.
    ///
    /// # Panics
    ///
    /// Panics if the data is not approximately equal.
    #[track_caller]
    pub fn assert_approx_eq(&self, other: &Self, precision: usize) {
        let tolerance = 0.1.pow(precision as f64);

        self.assert_approx_eq_diff(other, tolerance)
    }

    /// Asserts the data is approximately equal to another data.
    ///
    /// # Arguments
    ///
    /// * `other` - The other data.
    /// * `tolerance` - The tolerance of the comparison.
    ///
    /// # Panics
    ///
    /// Panics if the data is not approximately equal.
    #[track_caller]
    pub fn assert_approx_eq_diff(&self, other: &Self, tolerance: f64) {
        let mut message = String::new();
        if self.shape != other.shape {
            message += format!(
                "\n  => Shape is different: {:?} != {:?}",
                self.shape.dims, other.shape.dims
            )
            .as_str();
        }

        let iter = self.value.clone().into_iter().zip(other.value.clone());

        let mut num_diff = 0;
        let max_num_diff = 5;

        for (i, (a, b)) in iter.enumerate() {
            let a: f64 = a.into();
            let b: f64 = b.into();

            //if they are both nan, then they are equally nan
            let both_nan = a.is_nan() && b.is_nan();
            //this works for both infinities
            let both_inf = a.is_infinite() && b.is_infinite() && ((a > 0.) == (b > 0.));

            if both_nan || both_inf {
                continue;
            }

            let err = ((a - b).pow(2.0f64)).sqrt();

            if err > tolerance || err.is_nan() {
                // Only print the first 5 different values.
                if num_diff < max_num_diff {
                    message += format!(
                        "\n  => Position {i}: {a} != {b} | difference {err} > tolerance \
                         {tolerance}"
                    )
                    .as_str();
                }
                num_diff += 1;
            }
        }

        if num_diff >= max_num_diff {
            message += format!("\n{} more errors...", num_diff - 5).as_str();
        }

        if !message.is_empty() {
            panic!("Tensors are not approx eq:{}", message);
        }
    }
}

#[allow(deprecated)]
impl<const D: usize> Data<usize, D> {
    /// Converts the usize data to a different element type.
    pub fn from_usize<O: num_traits::FromPrimitive>(self) -> Data<O, D> {
        let value: Vec<O> = self
            .value
            .into_iter()
            .map(|a| num_traits::FromPrimitive::from_usize(a).unwrap())
            .collect();

        Data {
            value,
            shape: self.shape,
        }
    }
}

#[allow(deprecated)]
impl<E: Clone, const D: usize> From<&DataSerialize<E>> for Data<E, D> {
    fn from(data: &DataSerialize<E>) -> Self {
        let mut dims = [0; D];
        dims[..D].copy_from_slice(&data.shape[..D]);
        Data::new(data.value.clone(), Shape::new(dims))
    }
}

#[allow(deprecated)]
impl<E, const D: usize> From<DataSerialize<E>> for Data<E, D> {
    fn from(data: DataSerialize<E>) -> Self {
        let mut dims = [0; D];
        dims[..D].copy_from_slice(&data.shape[..D]);
        Data::new(data.value, Shape::new(dims))
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug + Copy, const A: usize> From<[E; A]> for Data<E, 1> {
    fn from(elems: [E; A]) -> Self {
        let mut data = Vec::with_capacity(2 * A);
        for elem in elems.into_iter() {
            data.push(elem);
        }

        Data::new(data, Shape::new([A]))
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug + Copy> From<&[E]> for Data<E, 1> {
    fn from(elems: &[E]) -> Self {
        let mut data = Vec::with_capacity(elems.len());
        for elem in elems.iter() {
            data.push(*elem);
        }

        Data::new(data, Shape::new([elems.len()]))
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug + Copy, const A: usize, const B: usize> From<[[E; B]; A]> for Data<E, 2> {
    fn from(elems: [[E; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B);
        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                data.push(elem);
            }
        }

        Data::new(data, Shape::new([A, B]))
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug + Copy, const A: usize, const B: usize, const C: usize>
    From<[[[E; C]; B]; A]> for Data<E, 3>
{
    fn from(elems: [[[E; C]; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B * C);

        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                for elem in elem.into_iter().take(C) {
                    data.push(elem);
                }
            }
        }

        Data::new(data, Shape::new([A, B, C]))
    }
}

#[allow(deprecated)]
impl<
        E: core::fmt::Debug + Copy,
        const A: usize,
        const B: usize,
        const C: usize,
        const D: usize,
    > From<[[[[E; D]; C]; B]; A]> for Data<E, 4>
{
    fn from(elems: [[[[E; D]; C]; B]; A]) -> Self {
        let mut data = Vec::with_capacity(A * B * C * D);

        for elem in elems.into_iter().take(A) {
            for elem in elem.into_iter().take(B) {
                for elem in elem.into_iter().take(C) {
                    for elem in elem.into_iter().take(D) {
                        data.push(elem);
                    }
                }
            }
        }

        Data::new(data, Shape::new([A, B, C, D]))
    }
}

#[allow(deprecated)]
impl<E: core::fmt::Debug, const D: usize> core::fmt::Display for Data<E, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(format!("{:?}", &self.value).as_str())
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use alloc::vec;
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn into_vec_should_yield_same_value_as_iter() {
        let shape = Shape::new([3, 5, 6]);
        let data = TensorData::random::<f32, _, _>(
            shape,
            Distribution::Default,
            &mut StdRng::from_entropy(),
        );

        let expected = data.iter::<f32>().collect::<Vec<f32>>();
        let actual = data.into_vec::<f32>().unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    #[should_panic]
    fn into_vec_should_assert_wrong_dtype() {
        let shape = Shape::new([3, 5, 6]);
        let data = TensorData::random::<f32, _, _>(
            shape,
            Distribution::Default,
            &mut StdRng::from_entropy(),
        );

        data.into_vec::<i32>().unwrap();
    }

    #[test]
    fn should_have_right_num_elements() {
        let shape = Shape::new([3, 5, 6]);
        let num_elements = shape.num_elements();
        let data = TensorData::random::<f32, _, _>(
            shape,
            Distribution::Default,
            &mut StdRng::from_entropy(),
        );

        assert_eq!(num_elements, data.bytes.len() / 4); // f32 stored as u8s
        assert_eq!(num_elements, data.as_slice::<f32>().unwrap().len());
    }

    #[test]
    fn should_have_right_shape() {
        let data = TensorData::from([[3.0, 5.0, 6.0]]);
        assert_eq!(data.shape, vec![1, 3]);

        let data = TensorData::from([[4.0, 5.0, 8.0], [3.0, 5.0, 6.0]]);
        assert_eq!(data.shape, vec![2, 3]);

        let data = TensorData::from([3.0, 5.0, 6.0]);
        assert_eq!(data.shape, vec![3]);
    }

    #[test]
    fn should_assert_appox_eq_limit() {
        let data1 = TensorData::from([[3.0, 5.0, 6.0]]);
        let data2 = TensorData::from([[3.01, 5.0, 6.0]]);

        data1.assert_approx_eq(&data2, 2);
    }

    #[test]
    #[should_panic]
    fn should_assert_appox_eq_above_limit() {
        let data1 = TensorData::from([[3.0, 5.0, 6.0]]);
        let data2 = TensorData::from([[3.011, 5.0, 6.0]]);

        data1.assert_approx_eq(&data2, 2);
    }

    #[test]
    #[should_panic]
    fn should_assert_appox_eq_check_shape() {
        let data1 = TensorData::from([[3.0, 5.0, 6.0, 7.0]]);
        let data2 = TensorData::from([[3.0, 5.0, 6.0]]);

        data1.assert_approx_eq(&data2, 2);
    }

    #[test]
    fn should_convert_bytes_correctly() {
        let mut vector: Vec<f32> = Vec::with_capacity(5);
        vector.push(2.0);
        vector.push(3.0);
        let data1 = TensorData::new(vector, vec![2]);

        let factor = core::mem::size_of::<f32>() / core::mem::size_of::<u8>();
        assert_eq!(data1.bytes.len(), 2 * factor);
        assert_eq!(data1.bytes.capacity(), 5 * factor);
    }

    #[test]
    fn should_convert_bytes_correctly_inplace() {
        fn test_precision<E: Element>() {
            let data = TensorData::new((0..32).collect(), [32]);
            for (i, val) in data
                .clone()
                .convert::<E>()
                .into_vec::<E>()
                .unwrap()
                .into_iter()
                .enumerate()
            {
                assert_eq!(i as u32, val.elem::<u32>())
            }
        }
        test_precision::<f32>();
        test_precision::<f16>();
        test_precision::<i64>();
        test_precision::<i32>();
    }

    #[test]
    fn should_pack_unpack_quantization_parameters_symmetric() {
        let scale = 0.03937008;
        // Quantized [[0.0, 1.0, 2.0], [3.0, 4.0, 5.0]]
        let data = TensorData::quantized(
            vec![0i8, 25, 51, 76, 102, 127],
            [2, 3],
            QuantizationStrategy::PerTensorSymmetricInt8(SymmetricQuantization::init(scale)),
        );

        let qparams = data.get_q_params::<f32, i8>().unwrap();

        assert_eq!(qparams.scale, scale);
        assert_eq!(qparams.offset, None);
    }

    #[test]
    fn should_pack_unpack_quantization_parameters_affine() {
        let scale = 0.019607844;
        let offset = -128;
        // Quantized [[0.0, 1.0, 2.0], [3.0, 4.0, 5.0]]
        let data = TensorData::quantized(
            vec![-128i8, -77, -26, 25, 76, 127],
            [2, 3],
            QuantizationStrategy::PerTensorAffineInt8(AffineQuantization::init(scale, offset)),
        );
        let qparams = data.get_q_params::<f32, i8>().unwrap();

        assert_eq!(qparams.scale, scale);
        assert_eq!(qparams.offset, Some(offset));
    }

    #[test]
    fn should_not_return_q_params() {
        let data = TensorData::from([[3.0, 5.0, 6.0, 7.0]]);
        assert!(data.get_q_params::<f32, i8>().is_none());
    }
    #[test]
    #[should_panic = "Expected quantized data"]
    fn should_not_dequantize() {
        let data = TensorData::from([[3.0, 5.0, 6.0, 7.0]]);
        data.dequantize().unwrap();
    }

    #[test]
    fn should_support_dequantize() {
        // Quantized [[0.0, 1.0, 2.0], [3.0, 4.0, 5.0]]
        let data = TensorData::quantized(
            vec![-128i8, -77, -26, 25, 76, 127],
            [2, 3],
            QuantizationStrategy::PerTensorAffineInt8(AffineQuantization::init(0.019607844, -128)),
        );

        let output = data.dequantize().unwrap();

        output.assert_approx_eq(&TensorData::from([[0.0, 1.0, 2.0], [3.0, 4.0, 5.0]]), 4);
    }
}
