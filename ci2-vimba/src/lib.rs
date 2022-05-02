#![cfg_attr(feature = "backtrace", feature(backtrace))]

#[cfg(feature = "backtrace")]
use std::backtrace::Backtrace;

use parking_lot::Mutex;
use std::{
    convert::TryInto,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use chrono::{DateTime, Utc};
use lazy_static::lazy_static;

use machine_vision_formats as formats;

use ci2::{AcquisitionMode, AutoMode, TriggerMode};
use formats::PixFmt;
use timestamped_frame::HostTimeData;

use basic_frame::DynamicFrame;
use channellib::{Receiver, Sender};

// Number of frames to allocate for the Vimba driver.
const N_BUFFER_FRAMES: usize = 10;
// Number of slots to allocate purely within rust.
const N_CHANNEL_FRAMES: usize = 10;

struct FrameSender {
    handle: CamHandle,
    tx: Sender<std::result::Result<DynamicFrame, ci2::Error>>,
}

struct CamHandle {
    inner: vimba_sys::VmbHandle_t,
}

unsafe impl Sync for CamHandle {}
unsafe impl Send for CamHandle {}

lazy_static! {
    // Prevent multiple concurrent access to structures and functions in Vimba
    // which are not threadsafe.
    static ref VIMBA_MUTEX: Mutex<()> = Mutex::new(());
    static ref IS_DONE: AtomicBool = AtomicBool::new(false);
    static ref SENDERS: Mutex<Vec<FrameSender>> = Mutex::new(Vec::new());
}

fn callback_rust(
    camera_handle: vimba_sys::VmbHandle_t,
    frame: *mut vimba_sys::VmbFrame_t,
) -> ci2::Result<()> {
    let now = chrono::Utc::now(); // earliest possible timestamp
    let frame_status = unsafe { (*frame).receiveStatus };
    if !IS_DONE.load(Ordering::Relaxed) {
        // Copy all data from Vimba.

        let msg = if frame_status == vimba_sys::VmbFrameStatusType::VmbFrameStatusComplete {
            // Make reference to image buffer.
            let buf_ref = unsafe {
                let buf_ref1 = (*frame).buffer;
                let buf_len = (*frame).bufferSize as usize;
                std::slice::from_raw_parts(buf_ref1 as *const u8, buf_len)
            };
            // Copy image buffer.
            let image_data = buf_ref.to_vec(); // makes copy

            // Copy other pieces of information.
            let code = unsafe { (*frame).pixelFormat };

            let flags = unsafe { (*frame).receiveFlags };
            let frame_id =
                if flags & vimba_sys::VmbFrameFlagsType::VmbFrameFlagsFrameID.0 as u32 != 0 {
                    unsafe { (*frame).frameID }
                } else {
                    eprintln!("no frame number data in frame");
                    0
                };

            let device_timestamp =
                if flags & vimba_sys::VmbFrameFlagsType::VmbFrameFlagsTimestamp.0 as u32 != 0 {
                    unsafe { (*frame).timestamp }
                } else {
                    eprintln!("no timestamp data in frame");
                    0
                };

            let pixel_format = vimba::pixel_format_code(code).map_vimba_err()?;

            {
                let extra = Box::new(VimbaExtra {
                    frame_id,
                    device_timestamp,
                    host_timestamp: now,
                    pixel_format,
                });

                let width = unsafe { (*frame).width };
                let height = unsafe { (*frame).height };
                let stride = unsafe { (*frame).width }; // TODO: need to scale by pixel n bytes?

                Ok(DynamicFrame::new(
                    width,
                    height,
                    stride, // need to scale by pixel n bytes?
                    extra,
                    image_data,
                    pixel_format,
                ))
            }
        } else {
            let str_msg = match frame_status {
                vimba_sys::VmbFrameStatusType::VmbFrameStatusIncomplete => {
                    "Frame could not be filled to the end"
                }
                vimba_sys::VmbFrameStatusType::VmbFrameStatusTooSmall => {
                    "Frame buffer was too small"
                }
                vimba_sys::VmbFrameStatusType::VmbFrameStatusInvalid => "Frame buffer was invalid",
                other => {
                    if other == -4 {
                        eprintln!("undocumented frame status -4: was VmbShutdown() called?");
                    }
                    panic!("undocumented frame status received {}", other);
                }
            };
            Err(ci2::Error::SingleFrameError(str_msg.into()))
        };

        // Enqueue frame again.
        let err = {
            let _guard = VIMBA_MUTEX.lock();
            unsafe { vimba_sys::VmbCaptureFrameQueue(camera_handle, frame, Some(callback_c)) }
        };

        if err != vimba_sys::VmbErrorType::VmbErrorSuccess {
            return Err(ci2::Error::BackendError(anyhow::anyhow!(
                "CB: capture error: {}",
                err
            )));
        }

        let tx = {
            // In this scope, we keep the lock on the SENDERS mutex.
            let vec_senders = &mut *SENDERS.lock();
            if let Some(idx) = vec_senders
                .iter()
                .position(|x| x.handle.inner == camera_handle)
            {
                let sender = &vec_senders[idx];
                sender.tx.clone()
            } else {
                return Err(ci2::Error::from(format!(
                    "CB: no sender found for camera: {:?}",
                    camera_handle
                )));
            }
        };

        match tx.send(msg) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("CB: send frame error: {}", e);
                IS_DONE.store(true, Ordering::Relaxed); // indicate we are done
            }
        }
    }
    Ok(())
}

/// # Safety
///
/// This function will not propagate panics that happen in the callback, but it
/// should print an error to stderr and then soon stop further image-ready
/// callbacks.
#[no_mangle]
pub unsafe extern "C" fn callback_c(
    camera_handle: vimba_sys::VmbHandle_t,
    frame: *mut vimba_sys::VmbFrame_t,
) {
    match std::panic::catch_unwind(|| {
        callback_rust(camera_handle, frame).unwrap();
    }) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("CB: Error: Panic {:?}", e);
            IS_DONE.store(true, Ordering::Relaxed); // indicate we are done.
        }
    }
}

trait ExtendedError<T> {
    fn map_vimba_err(self) -> ci2::Result<T>;
}

impl<T> ExtendedError<T> for std::result::Result<T, vimba::Error> {
    fn map_vimba_err(self) -> ci2::Result<T> {
        self.map_err(|e| ci2::Error::BackendError(e.into()))
    }
}

pub type Result<M> = std::result::Result<M, Error>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Vimba error: {source}")]
    VimbaError {
        #[from]
        source: vimba::Error,
        #[cfg(feature = "backtrace")]
        backtrace: Backtrace,
    },
}

impl From<Error> for ci2::Error {
    fn from(orig: Error) -> ci2::Error {
        ci2::Error::BackendError(orig.into())
    }
}

#[derive(Clone)]
pub struct WrappedModule {
    lib: Arc<vimba::VimbaLibrary>,
}

impl WrappedModule {
    fn camera_infos(&self) -> ci2::Result<Vec<VimbaCameraInfo>> {
        let n_cams = self.lib.n_cameras().map_vimba_err()?;
        let vimba_infos = self.lib.camera_info(n_cams).map_vimba_err()?;

        let infos = vimba_infos
            .into_iter()
            .map(|info| {
                let serial = info.serial_string;
                let model = info.camera_name;
                let vendor = "Allied Vision".to_string(); // TODO: read this
                let name = info.camera_id_string;
                VimbaCameraInfo {
                    name,
                    serial,
                    model,
                    vendor,
                }
            })
            .collect();
        Ok(infos)
    }
}

pub fn new_module() -> ci2::Result<WrappedModule> {
    let lib = vimba::VimbaLibrary::new()
        .map_err::<vimba::Error, _>(From::from)
        .map_err::<Error, _>(From::from)?;
    Ok(WrappedModule { lib: Arc::new(lib) })
}

impl<'a> ci2::CameraModule for &'a WrappedModule {
    type CameraType = WrappedCamera;

    fn name(self: &&'a WrappedModule) -> &'static str {
        "vimba"
    }
    fn camera_infos(self: &&'a WrappedModule) -> ci2::Result<Vec<Box<dyn ci2::CameraInfo>>> {
        let vec1 = WrappedModule::camera_infos(self)?;
        let infos = vec1
            .into_iter()
            .map(|vci| {
                let pci = Box::new(vci);
                let ci: Box<dyn ci2::CameraInfo> = pci; // explicitly perform type erasure
                ci
            })
            .collect();
        Ok(infos)
    }
    fn camera(self: &mut &'a WrappedModule, name: &str) -> ci2::Result<Self::CameraType> {
        let camera = vimba::Camera::open(name, vimba::access_mode::FULL).map_vimba_err()?;

        let vimba_infos = WrappedModule::camera_infos(self)?;
        let mut my_info = None;
        for ci in vimba_infos.into_iter() {
            if ci.name.as_str() == name {
                my_info = Some(ci);
                break;
            }
        }
        let info = my_info.unwrap();

        let rx = {
            // In this scope, we keep the lock on the SENDERS mutex.
            let vec_senders = &mut *SENDERS.lock();
            let (tx, rx) = channellib::bounded(N_CHANNEL_FRAMES);
            let sender = FrameSender {
                handle: CamHandle {
                    inner: camera.handle(),
                },
                tx,
            };
            vec_senders.push(sender);
            rx
        };

        let set_device_link_throughput_limit_mode =
            match std::env::var_os("DISABLE_SET_DEVICE_LINK_THROUGHPUT_LIMIT_MODE") {
                Some(v) => &v == "0",
                None => true,
            };

        if set_device_link_throughput_limit_mode {
            let mode = camera
                .feature_enum("DeviceLinkThroughputLimitMode")
                .map_vimba_err()?;

            if mode == "On" {
                camera
                    .feature_enum_set("DeviceLinkThroughputLimitMode", "Off")
                    .map_vimba_err()?;
            }
        }

        Ok(WrappedCamera {
            _lib: self.clone(),
            camera: Arc::new(Mutex::new(camera)),
            acquisition_started: false,
            info,
            frames: Vec::with_capacity(N_BUFFER_FRAMES),
            rx,
        })
    }

    fn settings_file_extension(&self) -> &str {
        "xml"
    }

    fn frame_info_extractor(&self) -> &'static dyn ci2::ExtractFrameInfo {
        &*FRAME_INFO
    }
}

lazy_static::lazy_static! {
    static ref FRAME_INFO: VimbaFrameInfo = VimbaFrameInfo {};
}

struct VimbaFrameInfo {}

impl ci2::ExtractFrameInfo for VimbaFrameInfo {
    fn extract_frame_info(
        &self,
        frame: &DynamicFrame,
    ) -> (
        Option<std::num::NonZeroU64>,
        Option<std::num::NonZeroU64>,
        usize,
        chrono::DateTime<chrono::Utc>,
    ) {
        use timestamped_frame::ExtraTimeData;
        let extra = frame.extra();

        let vimba_extra = extra.as_any().downcast_ref::<VimbaExtra>().unwrap();
        (
            std::num::NonZeroU64::new(vimba_extra.device_timestamp),
            std::num::NonZeroU64::new(vimba_extra.frame_id),
            extra.host_framenumber(),
            extra.host_timestamp(),
        )
    }
}

#[derive(Debug)]
pub struct VimbaCameraInfo {
    name: String,
    serial: String,
    model: String,
    vendor: String,
}

impl ci2::CameraInfo for VimbaCameraInfo {
    fn name(&self) -> &str {
        &self.name
    }
    fn serial(&self) -> &str {
        &self.serial
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn vendor(&self) -> &str {
        &self.vendor
    }
}

pub struct WrappedCamera {
    // We hold WrappedModule to prevent dropping it while the camera is open.
    _lib: WrappedModule,
    pub camera: Arc<Mutex<vimba::Camera>>,
    pub info: VimbaCameraInfo,
    acquisition_started: bool,
    frames: Vec<vimba::Frame>,
    rx: Receiver<std::result::Result<DynamicFrame, ci2::Error>>,
}

fn _test_camera_is_send() {
    // Compile-time test to ensure WrappedCamera implements Send trait.
    fn implements<T: Send>() {}
    implements::<WrappedCamera>();
}

impl ci2::CameraInfo for WrappedCamera {
    fn name(&self) -> &str {
        self.info.name()
    }
    fn serial(&self) -> &str {
        self.info.serial()
    }
    fn model(&self) -> &str {
        self.info.model()
    }
    fn vendor(&self) -> &str {
        self.info.vendor()
    }
}

impl ci2::Camera for WrappedCamera {
    // ----- start: weakly typed but easier to implement API -----

    // fn feature_access_query(&self, name: &str) -> ci2::Result<ci2::AccessQueryResult> {
    //     let (is_readable, is_writeable) = self
    //         .camera
    //         .lock()
    //         .feature_access_query(name)
    //         .map_vimba_err()?;
    //     Ok(ci2::AccessQueryResult {
    //         is_readable,
    //         is_writeable,
    //     })
    // }

    fn feature_enum_set(&self, name: &str, value: &str) -> ci2::Result<()> {
        self.camera
            .lock()
            .feature_enum_set(name, value)
            .map_vimba_err()
    }

    // ----- end: weakly typed but easier to implement API -----

    fn node_map_load(&self, settings: &str) -> std::result::Result<(), ci2::Error> {
        let dir = tempfile::tempdir()?;

        // write the settings to a file
        let settings_path = dir.path().join("settings.xml");
        {
            use std::io::Write;

            // The temporary file is open for writing in this scope.
            let mut file = std::fs::File::create(&settings_path)?;
            file.write_all(settings.as_bytes())?;
            file.flush()?;
            // When file goes out of scope, it will be closed.
        }

        let mut settings_settings = vimba::FeaturePersistentSettings::default(); // let's get meta. settings to load the settings.
        self.camera
            .lock()
            .camera_settings_load(&settings_path, &mut settings_settings)
            .map_vimba_err()

        // tempdir will be closed and removed when it is dropped.
    }

    fn node_map_save(&self) -> std::result::Result<String, ci2::Error> {
        let dir = tempfile::tempdir()?;

        // write the settings to a file
        let settings_path = dir.path().join("settings.xml");

        let mut settings_settings = vimba::FeaturePersistentSettings::default(); // let's get meta. settings to save the settings.
        self.camera
            .lock()
            .camera_settings_save(&settings_path, &mut settings_settings)
            .map_vimba_err()?;

        let buf = std::fs::read_to_string(&settings_path)?;
        Ok(buf)
        // tempdir will be closed and removed when it is dropped.
    }

    fn width(&self) -> std::result::Result<u32, ci2::Error> {
        Ok(self
            .camera
            .lock()
            .feature_int("Width")
            .map_vimba_err()?
            .try_into()?)
    }
    fn height(&self) -> std::result::Result<u32, ci2::Error> {
        Ok(self
            .camera
            .lock()
            .feature_int("Height")
            .map_vimba_err()?
            .try_into()?)
    }
    fn pixel_format(&self) -> std::result::Result<PixFmt, ci2::Error> {
        self.camera.lock().pixel_format().map_vimba_err()
    }
    fn possible_pixel_formats(&self) -> std::result::Result<Vec<PixFmt>, ci2::Error> {
        let fmts = self
            .camera
            .lock()
            .feature_enum_range_query("PixelFormat")
            .map_vimba_err()?;
        Ok(fmts
            .iter()
            // This silently drops pixel formats that cannot be converted.
            .filter_map(|fmt_str| vimba::str_to_pixel_format(fmt_str).map_vimba_err().ok())
            .into_iter()
            .collect())
    }
    fn set_pixel_format(&mut self, pixfmt: PixFmt) -> std::result::Result<(), ci2::Error> {
        let pixfmt_vimba = vimba::pixel_format_to_str(pixfmt).map_vimba_err()?;
        self.camera
            .lock()
            .feature_enum_set("PixelFormat", pixfmt_vimba)
            .map_vimba_err()?;
        Ok(())
    }
    fn exposure_time(&self) -> std::result::Result<f64, ci2::Error> {
        self.camera
            .lock()
            .feature_float("ExposureTime")
            .map_vimba_err()
    }
    fn exposure_time_range(&self) -> std::result::Result<(f64, f64), ci2::Error> {
        self.camera
            .lock()
            .feature_float_range_query("ExposureTime")
            .map_vimba_err()
    }
    fn set_exposure_time(&mut self, value: f64) -> std::result::Result<(), ci2::Error> {
        self.camera
            .lock()
            .feature_float_set("ExposureTime", value)
            .map_vimba_err()
    }
    fn exposure_auto(&self) -> std::result::Result<AutoMode, ci2::Error> {
        let c = self.camera.lock();
        let mystr = c.feature_enum("ExposureAuto").map_vimba_err()?;
        str_to_auto_mode(mystr)
    }
    fn set_exposure_auto(&mut self, value: AutoMode) -> std::result::Result<(), ci2::Error> {
        let valstr = auto_mode_to_str(value);
        let c = self.camera.lock();
        c.feature_enum_set("ExposureAuto", valstr).map_vimba_err()
    }
    fn gain(&self) -> std::result::Result<f64, ci2::Error> {
        self.camera.lock().feature_float("Gain").map_vimba_err()
    }
    fn gain_range(&self) -> std::result::Result<(f64, f64), ci2::Error> {
        self.camera
            .lock()
            .feature_float_range_query("Gain")
            .map_vimba_err()
    }
    fn set_gain(&mut self, value: f64) -> std::result::Result<(), ci2::Error> {
        self.camera
            .lock()
            .feature_float_set("Gain", value)
            .map_vimba_err()
    }
    fn gain_auto(&self) -> std::result::Result<AutoMode, ci2::Error> {
        let c = self.camera.lock();
        let mystr = c.feature_enum("GainAuto").map_vimba_err()?;
        str_to_auto_mode(mystr)
    }
    fn set_gain_auto(&mut self, value: AutoMode) -> std::result::Result<(), ci2::Error> {
        let valstr = auto_mode_to_str(value);
        let c = self.camera.lock();
        c.feature_enum_set("GainAuto", valstr).map_vimba_err()
    }

    fn start_default_external_triggering(&mut self) -> std::result::Result<(), ci2::Error> {
        let restart = if self.acquisition_started {
            self.acquisition_stop()?;
            true
        } else {
            false
        };

        // The trigger selector must be set before the trigger mode.
        self.set_trigger_selector(ci2::TriggerSelector::FrameStart)?;
        {
            let c = self.camera.lock();
            c.feature_enum_set("TriggerSource", "Line0")
                .map_vimba_err()?;
        }
        self.set_trigger_mode(ci2::TriggerMode::On)?;
        if restart {
            self.acquisition_start()?;
        }
        Ok(())
    }

    fn set_software_frame_rate_limit(
        &mut self,
        fps_limit: f64,
    ) -> std::result::Result<(), ci2::Error> {
        let restart = if self.acquisition_started {
            self.acquisition_stop()?;
            true
        } else {
            false
        };

        self.set_acquisition_frame_rate_enable(true)?;
        self.set_acquisition_frame_rate(fps_limit)?;

        if restart {
            self.acquisition_start()?;
        }
        Ok(())
    }

    fn trigger_mode(&self) -> std::result::Result<TriggerMode, ci2::Error> {
        let c = self.camera.lock();
        let val = c.feature_enum("TriggerMode").map_vimba_err()?;
        match val {
            "Off" => Ok(ci2::TriggerMode::Off),
            "On" => Ok(ci2::TriggerMode::On),
            s => {
                return Err(ci2::Error::from(format!(
                    "unexpected TriggerMode enum string: {}",
                    s
                )));
            }
        }
    }
    fn set_trigger_mode(&mut self, val: TriggerMode) -> std::result::Result<(), ci2::Error> {
        let valstr = match val {
            ci2::TriggerMode::Off => "Off",
            ci2::TriggerMode::On => "On",
        };
        let c = self.camera.lock();
        c.feature_enum_set("TriggerMode", valstr).map_vimba_err()
    }
    fn acquisition_frame_rate_enable(&self) -> std::result::Result<bool, ci2::Error> {
        self.camera
            .lock()
            .feature_boolean("AcquisitionFrameRateEnable")
            .map_vimba_err()
    }
    fn set_acquisition_frame_rate_enable(
        &mut self,
        value: bool,
    ) -> std::result::Result<(), ci2::Error> {
        self.camera
            .lock()
            .feature_boolean_set("AcquisitionFrameRateEnable", value)
            .map_vimba_err()
    }
    fn acquisition_frame_rate(&self) -> std::result::Result<f64, ci2::Error> {
        self.camera
            .lock()
            .feature_float("AcquisitionFrameRate")
            .map_vimba_err()
    }
    fn acquisition_frame_rate_range(&self) -> std::result::Result<(f64, f64), ci2::Error> {
        self.camera
            .lock()
            .feature_float_range_query("AcquisitionFrameRate")
            .map_vimba_err()
    }
    fn set_acquisition_frame_rate(&mut self, value: f64) -> std::result::Result<(), ci2::Error> {
        self.camera
            .lock()
            .feature_float_set("AcquisitionFrameRate", value)
            .map_vimba_err()
    }
    fn trigger_selector(&self) -> std::result::Result<ci2::TriggerSelector, ci2::Error> {
        let c = self.camera.lock();
        let val = c.feature_enum("TriggerSelector").map_vimba_err()?;
        match val {
            "AcquisitionStart" => Ok(ci2::TriggerSelector::AcquisitionStart),
            "FrameBurstStart" => Ok(ci2::TriggerSelector::FrameBurstStart),
            "FrameStart" => Ok(ci2::TriggerSelector::FrameStart),
            "ExposureActive" => Ok(ci2::TriggerSelector::ExposureActive),
            s => {
                return Err(ci2::Error::from(format!(
                    "unexpected TriggerSelector enum string: {}",
                    s
                )));
            }
        }
    }
    fn set_trigger_selector(
        &mut self,
        val: ci2::TriggerSelector,
    ) -> std::result::Result<(), ci2::Error> {
        let valstr = match val {
            ci2::TriggerSelector::AcquisitionStart => "AcquisitionStart",
            ci2::TriggerSelector::FrameStart => "FrameStart",
            ci2::TriggerSelector::FrameBurstStart => "FrameBurstStart",
            ci2::TriggerSelector::ExposureActive => "ExposureActive",
            _ => {
                return Err(ci2::Error::from(format!(
                    "unknown TriggerSelector mode: {:?}",
                    val
                )))
            }
        };
        let c = self.camera.lock();
        c.feature_enum_set("TriggerSelector", valstr)
            .map_vimba_err()
    }
    fn acquisition_mode(&self) -> std::result::Result<AcquisitionMode, ci2::Error> {
        let val = self
            .camera
            .lock()
            .feature_enum("AcquisitionMode")
            .map_vimba_err()?;
        Ok(match val {
            "Continuous" => AcquisitionMode::Continuous,
            "SingleFrame" => AcquisitionMode::SingleFrame,
            "MultiFrame" => AcquisitionMode::MultiFrame,
            val => {
                return Err(ci2::Error::from(format!(
                    "unknown AcquisitionMode: {:?}",
                    val
                )))
            }
        })
    }
    fn set_acquisition_mode(
        &mut self,
        value: AcquisitionMode,
    ) -> std::result::Result<(), ci2::Error> {
        let modes = self
            .camera
            .lock()
            .feature_enum_range_query("AcquisitionMode")
            .map_vimba_err()?;
        println!("modes {:?}", modes);

        let sval = match value {
            AcquisitionMode::Continuous => "Continuous",
            AcquisitionMode::SingleFrame => "SingleFrame",
            AcquisitionMode::MultiFrame => "MultiFrame",
        };
        self.camera
            .lock()
            .feature_enum_set("AcquisitionMode", sval)
            .map_vimba_err()
    }
    fn acquisition_start(&mut self) -> std::result::Result<(), ci2::Error> {
        IS_DONE.store(false, Ordering::Relaxed); // indicate we are done

        let camera = self.camera.lock();

        for _ in 0..N_BUFFER_FRAMES {
            let buffer = camera.allocate_buffer().map_vimba_err()?;
            let mut frame = vimba::Frame::new(buffer);
            camera.frame_announce(&mut frame).map_vimba_err()?;
            self.frames.push(frame);
        }

        // -----

        {
            let _guard = VIMBA_MUTEX.lock();

            camera.capture_start().map_vimba_err()?;

            for frame in self.frames.iter_mut() {
                camera
                    .capture_frame_queue_with_callback(frame, Some(callback_c))
                    .map_vimba_err()?;
            }

            camera.command_run("AcquisitionStart").map_vimba_err()?;
        }

        self.acquisition_started = true;
        Ok(())
    }
    fn acquisition_stop(&mut self) -> std::result::Result<(), ci2::Error> {
        let camera = self.camera.lock();

        IS_DONE.store(true, Ordering::Relaxed); // indicate we are done

        {
            let mut _guard = VIMBA_MUTEX.lock();
            camera.command_run("AcquisitionStop").map_vimba_err()?;
            camera.capture_end().map_vimba_err()?;
            camera.capture_queue_flush().map_vimba_err()?;
            for mut frame in self.frames.drain(..) {
                camera.frame_revoke(&mut frame).map_vimba_err()?;
            }
        }
        self.acquisition_started = false;
        Ok(())
    }
    fn next_frame(&mut self) -> std::result::Result<DynamicFrame, ci2::Error> {
        let msg = match self.rx.recv() {
            Ok(msg) => msg,
            Err(err) => {
                return Err(ci2::Error::BackendError(anyhow::anyhow!(
                    "Error receiving frame : {}",
                    err
                )));
            }
        };
        let frame = msg?;
        Ok(frame)
    }
}

#[derive(Clone, Debug)]
pub struct VimbaExtra {
    pub frame_id: u64,
    host_timestamp: DateTime<Utc>,
    pub pixel_format: formats::PixFmt,
    pub device_timestamp: u64,
}

impl HostTimeData for VimbaExtra {
    fn host_framenumber(&self) -> usize {
        // actually we just trust the device
        self.frame_id.try_into().unwrap()
    }
    fn host_timestamp(&self) -> DateTime<Utc> {
        self.host_timestamp
    }
}

fn str_to_auto_mode(val: &str) -> ci2::Result<ci2::AutoMode> {
    match val {
        "Off" => Ok(ci2::AutoMode::Off),
        "Once" => Ok(ci2::AutoMode::Once),
        "Continuous" => Ok(ci2::AutoMode::Continuous),
        s => {
            return Err(ci2::Error::from(format!(
                "unexpected AutoMode enum string: {}",
                s
            )));
        }
    }
}

fn auto_mode_to_str(value: ci2::AutoMode) -> &'static str {
    use ci2::AutoMode::*;
    match value {
        Off => "Off",
        Once => "Once",
        Continuous => "Continuous",
    }
}