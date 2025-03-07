//! Provides [`Trainer`], a simple interface for on-device training/fine-tuning.

use alloc::{
	ffi::CString,
	string::{String, ToString}
};
use core::{
	ffi::{CStr, c_char},
	marker::PhantomData,
	ptr::{self, NonNull}
};
use std::{path::Path, sync::OnceLock};

use crate::{
	AsPointer, Error, Result,
	memory::Allocator,
	ortsys,
	session::{NoSelectedOutputs, RunOptions},
	value::{DynTensor, Value, ValueType, ValueTypeMarker, r#type::extract_data_type_from_tensor_info}
};

mod simple;
mod trainer;

pub use self::{
	simple::{
		CheckpointStrategy, DataLoader, EvaluationStrategy, IterableDataLoader, TrainerCallbacks, TrainerControl, TrainerState, TrainingArguments,
		iterable_data_loader
	},
	trainer::Trainer
};

/// Returns a pointer to the global [`ort_sys::OrtTrainingApi`] object, or errors if the Training API is not enabled.
///
/// # Panics
/// May panic if:
/// - Getting the `OrtApi` struct fails, due to `ort` loading an unsupported version of ONNX Runtime.
/// - Loading the ONNX Runtime dynamic library fails if the `load-dynamic` feature is enabled.
pub fn training_api() -> Result<&'static ort_sys::OrtTrainingApi> {
	struct TrainingApiPointer(*const ort_sys::OrtTrainingApi);
	unsafe impl Send for TrainingApiPointer {}
	unsafe impl Sync for TrainingApiPointer {}

	static TRAINING_API: OnceLock<TrainingApiPointer> = OnceLock::new();

	let ptr = NonNull::new(
		TRAINING_API
			.get_or_init(|| {
				let training_api = ortsys![unsafe GetTrainingApi(ort_sys::ORT_API_VERSION)];
				TrainingApiPointer(training_api)
			})
			.0
			.cast_mut()
	)
	.ok_or_else(|| Error::new("Training is not enbled in this build of ONNX Runtime."))?;
	Ok(unsafe { ptr.as_ref() })
}

/// Sets the seed used for RNG when training.
pub fn set_seed(seed: i64) -> Result<()> {
	trainsys![unsafe SetSeed(seed)?];
	Ok(())
}

macro_rules! trainsys {
	($method:ident) => {
		($crate::training::training_api().unwrap().$method)
	};
	(unsafe $method:ident($($n:expr),+ $(,)?)) => {
		unsafe { ($crate::training::training_api().unwrap().$method)($($n),+) }
	};
	(unsafe $method:ident($($n:expr),+ $(,)?).expect($e:expr)) => {
		unsafe { $crate::error::status_to_result(($crate::training::training_api().unwrap().$method)($($n),+)) }.expect($e)
	};
	(unsafe $method:ident($($n:expr),+ $(,)?); nonNull($($check:expr),+ $(,)?)$(;)?) => {{
		let _x = unsafe { ($crate::training::training_api().unwrap().$method)($($n),+) };
		$(
			// TODO: #[cfg(debug_assertions)]?
			if ($check).is_null() {
				$crate::util::cold();
				panic!(concat!("expected `", stringify!($check), "` to not be null"));
			}
		)+
		_x
	}};
	(unsafe $method:ident($($n:expr),+ $(,)?)?) => {
		unsafe { $crate::error::status_to_result(($crate::training::training_api()?.$method)($($n),+)) }?
	};
	(unsafe $method:ident($($n:expr),+ $(,)?)?; nonNull($($check:expr),+ $(,)?)$(;)?) => {{
		unsafe { $crate::error::status_to_result(($crate::training::training_api()?.$method)($($n),+)) }?;
		$(
			// TODO: #[cfg(debug_assertions)]?
			if ($check).is_null() {
				$crate::util::cold();
				return Err($crate::Error::new(concat!("expected `", stringify!($check), "` to not be null")));
			}
		)+
	}};
}
pub(crate) use trainsys;

#[derive(Debug)]
pub struct Checkpoint {
	ptr: NonNull<ort_sys::OrtCheckpointState>
}

impl Checkpoint {
	pub fn load(path: impl AsRef<Path>) -> Result<Self> {
		let path = crate::util::path_to_os_char(path);
		let mut ptr: *mut ort_sys::OrtCheckpointState = ptr::null_mut();
		trainsys![unsafe LoadCheckpoint(path.as_ptr(), &mut ptr)?; nonNull(ptr)];
		Ok(Checkpoint {
			ptr: unsafe { NonNull::new_unchecked(ptr) }
		})
	}

	pub fn load_from_buffer(buffer: &[u8]) -> Result<Self> {
		let mut ptr: *mut ort_sys::OrtCheckpointState = ptr::null_mut();
		trainsys![unsafe LoadCheckpointFromBuffer(buffer.as_ptr().cast(), buffer.len(), &mut ptr)?; nonNull(ptr)];
		Ok(Checkpoint {
			ptr: unsafe { NonNull::new_unchecked(ptr) }
		})
	}

	pub fn save(&self, path: impl AsRef<Path>, include_optimizer_state: bool) -> Result<()> {
		let path = crate::util::path_to_os_char(path);
		trainsys![unsafe SaveCheckpoint(self.ptr.as_ptr(), path.as_ptr(), include_optimizer_state)?];
		Ok(())
	}

	pub fn add_property(&mut self, name: impl AsRef<str>, property: impl Into<Property>) -> Result<()> {
		let name = CString::new(name.as_ref())?;
		match property.into() {
			Property::Int(value) => {
				trainsys![unsafe AddProperty(self.ptr.as_ptr(), name.as_ptr(), ort_sys::OrtPropertyType::OrtIntProperty, (&value as *const i64).cast())?]
			}
			Property::Float(value) => {
				trainsys![unsafe AddProperty(self.ptr.as_ptr(), name.as_ptr(), ort_sys::OrtPropertyType::OrtFloatProperty, (&value as *const f32).cast())?]
			}
			Property::String(value) => {
				let value = CString::new(value)?;
				trainsys![unsafe AddProperty(self.ptr.as_ptr(), name.as_ptr(), ort_sys::OrtPropertyType::OrtStringProperty, value.as_ptr().cast())?]
			}
		}
		Ok(())
	}

	pub fn get_property(&self, name: impl AsRef<str>) -> Option<Property> {
		let name = CString::new(name.as_ref()).ok()?;
		let mut allocator = Allocator::default();
		let mut property_type: ort_sys::OrtPropertyType = ort_sys::OrtPropertyType::OrtIntProperty;
		let mut property_value: *const () = ptr::null();

		let status = trainsys![unsafe GetProperty(
			self.ptr.as_ptr(),
			name.as_ptr(),
			allocator.ptr_mut(),
			&mut property_type,
			&mut property_value
		)];
		unsafe { crate::error::status_to_result(status) }.ok()?;

		Some(match property_type {
			ort_sys::OrtPropertyType::OrtIntProperty => Property::Int(unsafe { *property_value.cast::<i64>() }),
			ort_sys::OrtPropertyType::OrtFloatProperty => Property::Float(unsafe { *property_value.cast::<f32>() }),
			ort_sys::OrtPropertyType::OrtStringProperty => {
				let value = unsafe { CStr::from_ptr(property_value.cast::<c_char>()) }.to_string_lossy().into();
				unsafe { allocator.free(property_value.cast_mut()) };
				Property::String(value)
			}
		})
	}

	pub fn get_parameter(&self, name: impl AsRef<str>, allocator: &Allocator) -> Result<DynTensor> {
		let name = CString::new(name.as_ref())?;

		let mut value_ptr = ptr::null_mut();
		trainsys![unsafe GetParameter(self.ptr.as_ptr(), name.as_ptr(), allocator.ptr().cast_mut(), &mut value_ptr)?; nonNull(value_ptr)];
		Ok(unsafe { DynTensor::from_ptr(NonNull::new_unchecked(value_ptr), None) })
	}

	pub fn update_parameter<T: ValueTypeMarker>(&mut self, name: impl AsRef<str>, value: &Value<T>) -> Result<()> {
		let name = CString::new(name.as_ref())?;
		trainsys![unsafe UpdateParameter(self.ptr.as_ptr(), name.as_ptr(), value.ptr().cast_mut())?];
		Ok(())
	}

	pub fn get_parameter_type(&self, name: impl AsRef<str>) -> Result<ValueType> {
		let name = CString::new(name.as_ref())?;

		let mut shape_info = ptr::null_mut();
		trainsys![unsafe GetParameterTypeAndShape(self.ptr.as_ptr(), name.as_ptr(), &mut shape_info)?; nonNull(shape_info)];
		let value_type = unsafe { extract_data_type_from_tensor_info(shape_info) };
		ortsys![unsafe ReleaseTensorTypeAndShapeInfo(shape_info)];
		Ok(value_type)
	}
}

#[derive(Debug, Clone, PartialEq)]
pub enum Property {
	Int(i64),
	Float(f32),
	String(String)
}

impl From<i64> for Property {
	fn from(value: i64) -> Self {
		Self::Int(value)
	}
}
impl From<f32> for Property {
	fn from(value: f32) -> Self {
		Self::Float(value)
	}
}
impl From<&str> for Property {
	fn from(value: &str) -> Self {
		Self::String(value.to_string())
	}
}
impl From<String> for Property {
	fn from(value: String) -> Self {
		Self::String(value)
	}
}

impl AsPointer for Checkpoint {
	type Sys = ort_sys::OrtCheckpointState;

	fn ptr(&self) -> *const Self::Sys {
		self.ptr.as_ptr()
	}
}

impl Drop for Checkpoint {
	fn drop(&mut self) {
		crate::trace!("dropping checkpoint");
		trainsys![unsafe ReleaseCheckpointState(self.ptr.as_ptr())];
	}
}

#[derive(Debug, Clone)]
pub enum LearningRateScheduler {
	Linear {
		warmup_step_count: i64,
		total_step_count: i64,
		initial_lr: f32
	}
}

#[derive(Debug)]
pub struct Optimizer<'s> {
	session: NonNull<ort_sys::OrtTrainingSession>,
	_p: PhantomData<&'s ()>
}

impl Optimizer<'_> {
	pub(crate) fn new(session: NonNull<ort_sys::OrtTrainingSession>) -> Self {
		Self { session, _p: PhantomData }
	}

	pub fn reset_grad(&mut self) -> Result<()> {
		trainsys![unsafe LazyResetGrad(self.session.as_ptr())?];
		Ok(())
	}

	pub fn lr(&self) -> Result<f32> {
		let mut lr = f32::NAN;
		trainsys![unsafe GetLearningRate(self.session.as_ptr(), &mut lr)?];
		Ok(lr)
	}

	pub fn set_lr(&mut self, lr: f32) -> Result<()> {
		trainsys![unsafe SetLearningRate(self.session.as_ptr(), lr)?];
		Ok(())
	}

	pub fn register_scheduler(&mut self, scheduler: LearningRateScheduler) -> Result<()> {
		match scheduler {
			LearningRateScheduler::Linear {
				warmup_step_count,
				total_step_count,
				initial_lr
			} => {
				trainsys![unsafe RegisterLinearLRScheduler(self.session.as_ptr(), warmup_step_count, total_step_count, initial_lr)?];
			}
		}
		Ok(())
	}

	pub fn step(&mut self) -> Result<()> {
		trainsys![unsafe OptimizerStep(self.session.as_ptr(), ptr::null_mut())?];
		Ok(())
	}

	pub fn step_with_options(&mut self, options: RunOptions<NoSelectedOutputs>) -> Result<()> {
		trainsys![unsafe OptimizerStep(self.session.as_ptr(), options.ptr())?];
		Ok(())
	}

	pub fn step_scheduler(&mut self) -> Result<()> {
		trainsys![unsafe SchedulerStep(self.session.as_ptr())?];
		Ok(())
	}
}
