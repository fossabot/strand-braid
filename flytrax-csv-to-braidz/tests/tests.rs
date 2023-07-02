use test_log::test;

use flytrax_csv_to_braidz::{parse_configs_and_run, RowFilter};

const INPUT_CSV: &str = include_str!("data/flytrax20191122_103500.csv");
const CALIBRATION_PARAMS_FILENAME: &str = "cal1.toml";
const CALIBRATION_PARAMS_TOML: &str = include_str!("data/cal1.toml");

#[test(tokio::test)]
async fn test_run_end_to_end() {
    let point_detection_csv_reader = INPUT_CSV.as_bytes();

    let flydra_csv_temp_dir = Some(tempfile::Builder::new().tempdir().unwrap());

    // Create unique dir for this test so we do not conflict with other
    // concurrent tests.
    let output_dir = tempfile::Builder::new().tempdir().unwrap();
    // The output .braidz filename:
    let output_braidz = output_dir.as_ref().join("out.braidz");

    let tracking_params_buf = Some(include_str!("data/tracking.toml"));

    let row_filters = vec![];
    parse_configs_and_run(
        point_detection_csv_reader,
        flydra_csv_temp_dir.as_ref(),
        None,
        &output_braidz,
        CALIBRATION_PARAMS_FILENAME,
        CALIBRATION_PARAMS_TOML,
        tracking_params_buf,
        &row_filters,
        true,
        None,
    )
    .await
    .unwrap();

    let reader = zip_or_dir::ZipDirArchive::auto_from_path(output_braidz).unwrap();
    let parsed = braidz_parser::braidz_parse(reader).unwrap();

    let kalman_estimates_info = parsed.kalman_estimates_info.as_ref().unwrap();

    assert!(kalman_estimates_info.trajectories.len() >= 7);
    assert!(kalman_estimates_info.trajectories.len() < 1000);

    flydra_csv_temp_dir.unwrap().close().unwrap();
    output_dir.close().unwrap();
}

#[test(tokio::test)]
async fn test_z_values_zero() {
    let point_detection_csv_reader = INPUT_CSV.as_bytes();

    let flydra_csv_temp_dir = Some(tempfile::Builder::new().tempdir().unwrap());

    // Create unique dir for this test so we do not conflict with other
    // concurrent tests.
    let output_dir = tempfile::Builder::new().tempdir().unwrap();
    // The output .braidz filename:
    let output_braidz = output_dir.as_ref().join("out.braidz");

    let row_filters = vec![RowFilter::InPseudoCalRegion];
    parse_configs_and_run(
        point_detection_csv_reader,
        flydra_csv_temp_dir.as_ref(),
        None,
        &output_braidz,
        CALIBRATION_PARAMS_FILENAME,
        CALIBRATION_PARAMS_TOML,
        None,
        &row_filters,
        true,
        None,
    )
    .await
    .unwrap();

    let reader = zip_or_dir::ZipDirArchive::auto_from_path(output_braidz).unwrap();
    let parsed = braidz_parser::braidz_parse(reader).unwrap();

    let kalman_estimates_info = parsed.kalman_estimates_info.as_ref().unwrap();
    let trajs = &kalman_estimates_info.trajectories;

    let mut count = 0;
    for traj_data in trajs.values() {
        for row in traj_data.position.iter() {
            count += 1;
            assert!(row[2].abs() < 1e-6);
        }
    }

    assert!(count >= 1);

    flydra_csv_temp_dir.unwrap().close().unwrap();

    // Delete the temporary directory.
    output_dir.close().unwrap();
}
