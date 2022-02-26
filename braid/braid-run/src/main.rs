#[macro_use]
extern crate log;

// For some reason, using Jemalloc prevents "corrupted size vs prev_size" error.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

use anyhow::Result;
use structopt::StructOpt;

use flydra_types::{
    MainbrainBuiLocation, RawCamName, TriggerType,
};
// use strand_cam::ImPtDetectCfgSource;

use braid::braid_start;
use braid_config_data::parse_config_file;
use flydra_types::BraidCameraConfig;

#[derive(Debug, StructOpt)]
#[structopt(about = "run the multi-camera realtime 3D tracker")]
struct BraidRunCliArgs {
    /// Input directory
    #[structopt(parse(from_os_str))]
    config_file: std::path::PathBuf,
}

fn launch_strand_cam(
    camera: BraidCameraConfig,
    mainbrain_internal_addr: MainbrainBuiLocation,
) -> Result<()> {

    // On initial startup strand cam queries for
    // [flydra_types::RemoteCameraInfoResponse] and thus we do not need to
    // provide much info.

    let base_url = mainbrain_internal_addr.0.base_url();

    let braid_run_exe = std::env::current_exe().unwrap();
    let exe_dir = braid_run_exe.parent().expect("Executable must be in some directory");
    #[cfg(target_os="windows")]
    let ext = ".exe";
    #[cfg(not(target_os="windows"))]
    let ext = "";
    let exe = exe_dir.join(format!("strand-cam-{}{}", camera.backend.as_str(), ext));
    debug!("strand cam executable name: \"{}\"", exe.display());

    let mut exec = std::process::Command::new(exe);
    exec.args(["--camera-name",
        &camera.name,
        "--http-server-addr",
        "127.0.0.1:0",
        "--no-browser",
        "--braid_addr",
        &base_url]);
    debug!("exec: {:?}", exec);
    let mut obj = exec
        .spawn()
        .unwrap();
    debug!("obj: {:?}", obj);

    std::thread::spawn(move || {
        let exit_code = obj.wait().unwrap();
        debug!("done. exit_code: {:?}", exit_code);
    });

    Ok(())
}

fn main() -> Result<()> {
    braid_start("run")?;

    let args = BraidRunCliArgs::from_args();
    debug!("{:?}", args);

    let cfg = parse_config_file(&args.config_file)?;
    debug!("{:?}", cfg);

    let n_local_cameras = cfg.cameras.iter().filter(|c| !c.remote_camera).count();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4 + 4 * n_local_cameras)
        .thread_name("braid-runtime")
        .thread_stack_size(3 * 1024 * 1024)
        .build()?;

    let pixel_formats = cfg
        .cameras
        .iter()
        .map(|cfg| (cfg.name.clone(), cfg.clone()))
        .collect();

    let trig_cfg = cfg.trigger;

    let (force_camera_sync_mode, software_limit_framerate) = match &trig_cfg {
        TriggerType::TriggerboxV1(_) => (true, flydra_types::StartSoftwareFrameRateLimit::NoChange),
        TriggerType::FakeSync(cfg) => (
            false,
            flydra_types::StartSoftwareFrameRateLimit::Enable(cfg.framerate),
        ),
    };
    let show_tracking_params = false;

    let handle = runtime.handle().clone();
    let all_expected_cameras = cfg
        .cameras
        .iter()
        .map(|x| RawCamName::new(x.name.clone()).to_ros())
        .collect();
    let phase1 = runtime.block_on(flydra2_mainbrain::pre_run(
        &handle,
        show_tracking_params,
        // Raising the mainbrain thread priority is currently disabled.
        // cfg.mainbrain.sched_policy_priority,
        pixel_formats,
        trig_cfg,
        &cfg.mainbrain,
        cfg.mainbrain
            .jwt_secret
            .as_ref()
            .map(|x| x.as_bytes().to_vec()),
        all_expected_cameras,
        force_camera_sync_mode,
        software_limit_framerate.clone(),
        "braid",
    ))?;

    let mainbrain_server_info = MainbrainBuiLocation(phase1.mainbrain_server_info.clone());

    let cfg_cameras = cfg.cameras;

    let _enter_guard = runtime.enter();
    let _strand_cams = cfg_cameras
        .into_iter()
        .filter_map(|camera| {
            if !camera.remote_camera {
                Some(launch_strand_cam(
                    camera,
                    mainbrain_server_info.clone(),
                ))
            } else {
                log::info!("Not starting remote camera \"{}\"", camera.name);
                None
            }
        })
        .collect::<Result<Vec<()>>>()?;

    debug!("done launching cameras");

    // This runs the whole thing and blocks.
    runtime.block_on(flydra2_mainbrain::run(phase1))?;

    // Now wait for everything to end..

    debug!("done {}:{}", file!(), line!());

    Ok(())
}
