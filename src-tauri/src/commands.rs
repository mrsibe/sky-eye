use crate::astrometry::{matcher, platesolve};
use crate::catalog::refcat2::Refcat2Client;
use crate::catalog::vizier::{GaiaQuery, GaiaQueryResult, VizierClient};
use crate::core::{AstroTime, FrameId};
use crate::ephemeris::{
    angular_distance_arcsec, propagate, propagate_with_context, EphemerisPoint, Observatory,
    PropagationContext,
};
use crate::fits;
use crate::measurement::{
    normalize_tracklet_designation, CandidateMatch, MatchStatus, MeasureTargetRequest,
    TargetMeasurement, TrackletMatchResult, TrackletPoint,
};
use crate::mpcorb::MpcorbManifest;
use crate::reduction::{
    mark_saturated_sources, refine_astrometric_centroids, select_astrometry_sources,
    ApertureConfig, ApertureMeasurement, ApertureRequest, BackgroundConfig, DetectionConfig,
    ReductionBackend, SepReducer, SourceMeasurement,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::ipc::{Channel, Response};
use tauri::State;
use tokio_util::sync::CancellationToken;

pub struct AppState {
    pub loaded_frames: Arc<Mutex<fits::registry::FrameRegistry>>,
    pub frame_analyses: Mutex<Vec<FrameAnalysis>>,
    pub current_frame_index: Mutex<usize>,
    pub blink_playing: Mutex<bool>,
    pub blink_speed_ms: Mutex<u64>,
    pub vizier: VizierClient,
    pub refcat2: Refcat2Client,
    pub jpl: Arc<crate::ephemeris::jpl::JplClient>,
    pub catalog_cancellations: Mutex<HashMap<String, CancellationToken>>,
    pub pending_measurements: Mutex<HashMap<String, TargetMeasurement>>,
    pub measurements: Mutex<Vec<TargetMeasurement>>,
    pub reduction_runs: Mutex<HashMap<String, serde_json::Value>>,
    pub mpcorb_cache: Mutex<Option<(MpcorbManifest, Arc<Vec<crate::mpcorb::OrbitRecord>>)>>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            loaded_frames: Arc::new(Mutex::new(fits::registry::FrameRegistry::default())),
            frame_analyses: Mutex::new(Vec::new()),
            current_frame_index: Mutex::new(0),
            blink_playing: Mutex::new(false),
            blink_speed_ms: Mutex::new(300),
            vizier: VizierClient::default(),
            refcat2: Refcat2Client::default(),
            jpl: Arc::new(crate::ephemeris::jpl::JplClient::default()),
            catalog_cancellations: Mutex::new(HashMap::new()),
            pending_measurements: Mutex::new(HashMap::new()),
            measurements: Mutex::new(Vec::new()),
            reduction_runs: Mutex::new(HashMap::new()),
            mpcorb_cache: Mutex::new(None),
        }
    }
}

#[tauri::command]
pub fn close_all_images(state: State<AppState>) -> Result<(), String> {
    state
        .loaded_frames
        .lock()
        .map_err(|e| e.to_string())?
        .clear();
    state
        .frame_analyses
        .lock()
        .map_err(|e| e.to_string())?
        .clear();
    state
        .pending_measurements
        .lock()
        .map_err(|e| e.to_string())?
        .clear();
    state
        .measurements
        .lock()
        .map_err(|e| e.to_string())?
        .clear();
    state
        .reduction_runs
        .lock()
        .map_err(|e| e.to_string())?
        .clear();
    *state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())? = 0;
    *state.blink_playing.lock().map_err(|e| e.to_string())? = false;
    *state.mpcorb_cache.lock().map_err(|e| e.to_string())? = None;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectionResult {
    pub stars: Vec<SourceMeasurement>,
    pub astrometry_stars: Vec<SourceMeasurement>,
    pub noise: f32,
    pub background: f32,
    pub num_stars: u32,
    pub backend: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FrameAnalysis {
    pub detection_revision: Option<String>,
    pub detection: Option<DetectionResult>,
    pub catalog: Option<GaiaQueryResult>,
    pub solution: Option<platesolve::PlateSolveResult>,
    pub photometry: Option<FramePhotometry>,
    pub photometry_error: Option<String>,
}

const DETECTION_ALGORITHM_VERSION: &str = "sep-1.3-gaussian-window-v2";

#[derive(Serialize)]
struct DetectionRevisionInput<'a> {
    frame: &'a fits::registry::FrameIdentity,
    threshold_sigma: f32,
    minimum_fwhm_px: f64,
    maximum_reference_stars: usize,
    saturation_level: Option<f64>,
    saturation_source: &'static str,
    algorithm_version: &'static str,
}

fn detection_revision(input: &DetectionRevisionInput<'_>) -> Result<String, String> {
    let bytes = serde_json::to_vec(input).map_err(|error| error.to_string())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug, Clone, Serialize)]
pub struct FramePhotometry {
    pub solution: crate::catalog::refcat2::PhotometricSolution,
    pub catalog_sha256: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SolveParams {
    pub center_ra_deg: Option<f64>,
    pub center_dec_deg: Option<f64>,
    pub radius_deg: Option<f64>,
    pub pixel_scale_arcsec: Option<f64>,
    pub rotation_deg: Option<f64>,
    pub parity_flipped: Option<bool>,
    pub offset_x_px: Option<f64>,
    pub offset_y_px: Option<f64>,
    pub catalog_bright_limit_mag: Option<f32>,
    pub catalog_faint_limit_mag: Option<f32>,
    pub maximum_reference_stars: Option<usize>,
    pub astrometric_residual_limit_arcsec: Option<f64>,
    pub plate_model: Option<matcher::PlateModel>,
    /// An explicit operator decision to accept a statistically sound solution
    /// that did not meet the automatic reference-count/coverage gate.
    pub accept_review: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameReductionResult {
    pub frame_index: usize,
    pub detection: Option<DetectionResult>,
    pub solution: platesolve::PlateSolveResult,
    pub photometry: Option<FramePhotometry>,
    pub photometry_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchReductionResult {
    pub frames: Vec<FrameReductionResult>,
    pub solved: usize,
    pub failed: usize,
    pub session_id: String,
    pub session_log_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchReductionProgress {
    pub frame_index: usize,
    pub total: usize,
    pub phase: &'static str,
    pub message: String,
}

fn publish_reduction_progress(
    on_progress: &Channel<BatchReductionProgress>,
    session: &mut Option<crate::logging::ReductionSessionLog>,
    run_id: &str,
    progress: BatchReductionProgress,
) {
    let level = if progress.phase == "failed" {
        log::Level::Warn
    } else {
        log::Level::Info
    };
    log::log!(
        level,
        "run={run_id} frame={}/{} phase={} {}",
        progress.frame_index + 1,
        progress.total,
        progress.phase,
        progress.message
    );
    if let Some(session) = session.as_mut() {
        session.line(
            level,
            &format!(
                "frame {}/{} {}",
                progress.frame_index + 1,
                progress.total,
                progress.phase
            ),
            &progress.message,
        );
    }
    if let Err(error) = on_progress.send(progress) {
        log::warn!("run={run_id} failed to deliver reduction progress: {error}");
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartReductionRequest {
    pub params: SolveParams,
}

#[derive(Debug, Deserialize)]
pub struct FrontendLogEntry {
    pub level: String,
    pub message: String,
    pub context: Option<String>,
}

#[tauri::command]
pub fn write_frontend_log(entry: FrontendLogEntry) -> Result<(), String> {
    let mut message = entry.message;
    if message.len() > 16 * 1024 {
        message.truncate(16 * 1024);
        message.push_str(" [truncated]");
    }
    let context = entry.context.unwrap_or_else(|| "app".to_string());
    match entry.level.to_ascii_lowercase().as_str() {
        "debug" => log::debug!(target: "frontend", "[{context}] {message}"),
        "warn" => log::warn!(target: "frontend", "[{context}] {message}"),
        "error" => log::error!(target: "frontend", "[{context}] {message}"),
        _ => log::info!(target: "frontend", "[{context}] {message}"),
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ApertureParams {
    pub x: f64,
    pub y: f64,
    pub radius: Option<f64>,
    pub annulus_inner: Option<f64>,
    pub annulus_outer: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FrameMeta {
    pub id: FrameId,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub min_val: f32,
    pub max_val: f32,
    pub object: Option<String>,
    pub ra: Option<f64>,
    pub dec: Option<f64>,
    pub exposure: Option<f64>,
    pub filter: Option<String>,
    pub date_obs: Option<String>,
    pub selected_hdu: usize,
    pub image_hdu_count: usize,
    pub timesys: String,
    pub time_reference: Option<String>,
    pub diagnostics: Vec<String>,
    pub observation_midpoint_jd: Option<f64>,
    pub focal_length: Option<f64>,
    pub pixel_size: Option<f64>,
    pub pixel_scale_arcsec: Option<f64>,
    pub rotation_deg: Option<f64>,
    pub parity_flipped: Option<bool>,
    pub label: String,
    pub solved: bool,
}

#[derive(Debug, Serialize)]
pub struct LoadFramesResult {
    pub frames: Vec<FrameMeta>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct BlinkState {
    pub current_index: usize,
    pub playing: bool,
    pub speed_ms: u64,
}

#[tauri::command]
pub async fn load_frames(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<LoadFramesResult, String> {
    state
        .pending_measurements
        .lock()
        .map_err(|error| error.to_string())?
        .clear();
    state
        .measurements
        .lock()
        .map_err(|error| error.to_string())?
        .clear();
    state
        .reduction_runs
        .lock()
        .map_err(|error| error.to_string())?
        .clear();
    let settings = crate::storage::load_config(&app)?;
    let exposure_factor = match settings.time.exposure_unit.as_str() {
        "milliseconds" => 0.001,
        "minutes" => 60.0,
        _ => 1.0,
    };
    let load_paths = paths.clone();
    let time_settings = settings.time.clone();
    let registry = tokio::task::spawn_blocking(move || {
        let mut registry = fits::registry::FrameRegistry::default();
        for path in load_paths {
            let mut frame = fits::reader::load_fits(&path)?;
            frame.metadata.exposure = frame.metadata.exposure.map(|value| value * exposure_factor);
            if let Some(value) = frame.metadata.date_obs.clone() {
                let exposure = frame.metadata.exposure.unwrap_or(0.0);
                let reference = frame.metadata.time_reference.clone();
                frame.metadata.observation_midpoint_jd =
                    configured_midpoint_jd(&value, exposure, &time_settings, reference.as_deref());
                frame.metadata.observation_midpoint_utc =
                    midpoint_rfc3339(&value, exposure, &time_settings, reference.as_deref());
            }
            registry.push_loaded(frame)?;
        }
        Ok::<_, String>(registry)
    })
    .await
    .map_err(|e| format!("FITS loader worker failed: {e}"))??;
    let mut metas: Vec<FrameMeta> = Vec::new();
    for (frame_index, (path, data)) in paths.iter().zip(registry.summaries()).enumerate() {
        let label = data.metadata.object.clone().unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
        metas.push(FrameMeta {
            id: FrameId(frame_index as u64),
            path: path.clone(),
            width: data.width,
            height: data.height,
            min_val: data.min,
            max_val: data.max,
            object: data.metadata.object.clone(),
            ra: data.metadata.ra,
            dec: data.metadata.dec,
            exposure: data.metadata.exposure,
            filter: data.metadata.filter.clone(),
            date_obs: data.metadata.date_obs.clone(),
            selected_hdu: data.metadata.selected_hdu,
            image_hdu_count: data.metadata.image_hdu_count,
            timesys: data.metadata.timesys.clone(),
            time_reference: data.metadata.time_reference.clone(),
            diagnostics: data.metadata.diagnostics.clone(),
            observation_midpoint_jd: data.metadata.observation_midpoint_jd,
            focal_length: data.metadata.focal_len,
            pixel_size: data.metadata.pixel_size,
            pixel_scale_arcsec: data.metadata.pixel_scale_arcsec,
            rotation_deg: data.metadata.rotation_deg,
            parity_flipped: data.metadata.parity_flipped,
            label,
            solved: false,
        });
    }
    let total = registry.len();
    *state.loaded_frames.lock().map_err(|e| e.to_string())? = registry;
    let mut analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
    *analyses = vec![FrameAnalysis::default(); total];
    let mut idx = state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    *idx = 0;

    Ok(LoadFramesResult {
        frames: metas,
        total,
    })
}

#[tauri::command]
pub async fn get_frame_pixel_buffer(
    state: State<'_, AppState>,
    index: usize,
) -> Result<Response, String> {
    // 克隆 Arc 以便移入 blocking 闭包;锁只在闭包内、绝不在 await 上持有。
    let registry = state.loaded_frames.clone();
    let bytes: Vec<u8> = tauri::async_runtime::spawn_blocking(move || {
        // 锁缩小到只取 Arc<FitsData>(owned),48~64MiB 字节拷贝在锁外进行。
        let data = {
            let mut frames = registry.lock().map_err(|e| e.to_string())?;
            frames.get(index)?
        };
        let bytes = {
            #[cfg(target_endian = "little")]
            {
                bytemuck::cast_slice::<f32, u8>(&data.pixels).to_vec()
            }
            #[cfg(target_endian = "big")]
            {
                data.pixels.iter().flat_map(|p| p.to_le_bytes()).collect()
            }
        };
        Ok::<_, String>(bytes)
    })
    .await
    .map_err(|e| format!("pixel-buffer worker failed: {e}"))??;
    Ok(Response::new(bytes))
}

#[tauri::command]
pub async fn detect_stars(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<DetectionResult, String> {
    let idx = *state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    let settings = crate::storage::load_config(&app)?;
    detect_frame_async(
        &state,
        idx,
        settings.reduction.detection_sigma as f32,
        settings.reduction.minimum_fwhm_px,
        settings.reduction.maximum_reference_stars as usize,
        settings.instrument.saturation_adu,
    )
    .await
}

async fn detect_frame_async(
    state: &AppState,
    idx: usize,
    threshold_sigma: f32,
    minimum_fwhm_px: f64,
    maximum_reference_stars: usize,
    saturation_override: Option<f64>,
) -> Result<DetectionResult, String> {
    let (
        mut pixels,
        valid_pixels,
        width,
        height,
        saturation_level,
        saturation_source,
        gain_e_per_adu,
        identity,
    ) = {
        let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
        let data = frames.get(idx)?;
        let identity = frames.identity(idx)?.clone();
        let (saturation, source) = match data.metadata.saturation_level {
            Some(value) => (Some(value), "fits_header"),
            None if saturation_override.is_some() => (saturation_override, "instrument_config"),
            None => (None, "none"),
        };
        (
            data.pixels.clone(),
            data.valid_pixels.clone(),
            data.width,
            data.height,
            saturation,
            source,
            data.metadata.gain_e_per_adu,
            identity,
        )
    };
    let revision = detection_revision(&DetectionRevisionInput {
        frame: &identity,
        threshold_sigma,
        minimum_fwhm_px,
        maximum_reference_stars,
        saturation_level,
        saturation_source,
        algorithm_version: DETECTION_ALGORITHM_VERSION,
    })?;
    {
        let mut analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
        let analysis = analyses
            .get_mut(idx)
            .ok_or("Invalid frame analysis index")?;
        if analysis.detection_revision.as_deref() == Some(&revision) {
            if let Some(cached) = analysis.detection.clone() {
                return Ok(cached);
            }
        } else if analysis.detection_revision.is_some() || analysis.detection.is_some() {
            *analysis = FrameAnalysis::default();
        }
    }
    mark_frame_measurements_stale(state, idx, "检测参数或输入文件标识已变化")?;
    let result = tokio::task::spawn_blocking(move || {
        fill_invalid_pixels(&mut pixels, &valid_pixels);
        let reducer = SepReducer;
        let background = reducer
            .background(&pixels, width, height, BackgroundConfig::default())
            .map_err(|error| error.to_string())?;
        let mut stars = reducer
            .detect(
                &pixels,
                width,
                height,
                &background,
                DetectionConfig {
                    threshold_sigma,
                    ..DetectionConfig::default()
                },
            )
            .map_err(|error| error.to_string())?;
        let saturated_count =
            mark_saturated_sources(&mut stars, &pixels, width, height, saturation_level);
        log::debug!(
            "[sky-eye][sources] saturation: effective={saturation_level:?} rejected={saturated_count}"
        );
        refine_astrometric_centroids(
            &mut stars,
            &pixels,
            &valid_pixels,
            width,
            height,
            &background,
            gain_e_per_adu,
        );
        stars.retain(|star| star.fwhm >= minimum_fwhm_px);
        let num_stars = stars.len() as u32;
        let astrometry_stars = select_astrometry_sources(
            &stars,
            width,
            height,
            maximum_reference_stars.clamp(8, 500),
        );
        Ok::<_, String>(DetectionResult {
            stars,
            astrometry_stars,
            noise: background.global_rms,
            background: background.global,
            num_stars,
            backend: "SEP 1.3",
        })
    })
    .await
    .map_err(|error| format!("source-detection worker failed: {error}"))??;
    let mut analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
    let analysis = analyses
        .get_mut(idx)
        .ok_or("Invalid frame analysis index")?;
    analysis.detection_revision = Some(revision);
    analysis.detection = Some(result.clone());
    Ok(result)
}

fn mark_frame_measurements_stale(
    state: &AppState,
    frame_index: usize,
    reason: &str,
) -> Result<(), String> {
    for measurement in state
        .pending_measurements
        .lock()
        .map_err(|error| error.to_string())?
        .values_mut()
        .filter(|measurement| measurement.frame_index == frame_index)
    {
        measurement.stale = true;
        measurement.stale_reason = Some(reason.to_string());
    }
    for measurement in state
        .measurements
        .lock()
        .map_err(|error| error.to_string())?
        .iter_mut()
        .filter(|measurement| measurement.frame_index == frame_index)
    {
        measurement.stale = true;
        measurement.stale_reason = Some(reason.to_string());
    }
    Ok(())
}

fn fill_invalid_pixels(pixels: &mut [f32], valid_pixels: &[bool]) {
    if pixels.len() != valid_pixels.len() {
        return;
    }
    let mut sample: Vec<f32> = pixels
        .iter()
        .zip(valid_pixels)
        .filter_map(|(pixel, valid)| (*valid && pixel.is_finite()).then_some(*pixel))
        .step_by((pixels.len() / 16_384).max(1))
        .collect();
    sample.sort_by(f32::total_cmp);
    let replacement = sample.get(sample.len() / 2).copied().unwrap_or(0.0);
    for (pixel, valid) in pixels.iter_mut().zip(valid_pixels) {
        if !*valid || !pixel.is_finite() {
            *pixel = replacement;
        }
    }
}

#[tauri::command]
pub fn get_frame_analysis(state: State<AppState>, index: usize) -> Result<FrameAnalysis, String> {
    state
        .frame_analyses
        .lock()
        .map_err(|e| e.to_string())?
        .get(index)
        .cloned()
        .ok_or_else(|| "Invalid frame analysis index".to_string())
}

#[tauri::command]
pub async fn query_gaia(
    state: State<'_, AppState>,
    request_id: String,
    query: GaiaQuery,
) -> Result<GaiaQueryResult, String> {
    if request_id.trim().is_empty() || request_id.len() > 128 {
        return Err("request_id must contain 1 to 128 characters".to_string());
    }
    let token = CancellationToken::new();
    state
        .catalog_cancellations
        .lock()
        .map_err(|error| error.to_string())?
        .insert(request_id.clone(), token.clone());
    let result = state.vizier.query_gaia(query, token).await;
    state
        .catalog_cancellations
        .lock()
        .map_err(|error| error.to_string())?
        .remove(&request_id);
    result.map_err(|error| error.to_string())
}

#[tauri::command]
pub fn cancel_gaia_query(state: State<AppState>, request_id: String) -> Result<bool, String> {
    let cancellation = state
        .catalog_cancellations
        .lock()
        .map_err(|error| error.to_string())?
        .remove(&request_id);
    if let Some(token) = cancellation {
        token.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
pub async fn query_refcat2(
    state: State<'_, AppState>,
    query: crate::catalog::refcat2::Refcat2Query,
) -> Result<crate::catalog::refcat2::Refcat2Result, String> {
    state.refcat2.query(query).await
}
#[derive(Debug, Deserialize)]
pub struct PhotometryFitRequest {
    pub samples: Vec<(f64, f64, f64)>,
    pub band: String,
}
#[tauri::command]
pub fn fit_photometry(
    request: PhotometryFitRequest,
) -> Result<crate::catalog::refcat2::PhotometricSolution, String> {
    crate::catalog::refcat2::robust_fit(&request.samples, &request.band)
}

#[derive(Debug, Deserialize)]
pub struct CalibratePhotometryRequest {
    pub frame_index: usize,
    pub band: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct CalibratePhotometryResult {
    pub solution: crate::catalog::refcat2::PhotometricSolution,
    pub catalog_sha256: String,
    pub matched_reference_stars: usize,
    pub measurements: Vec<TargetMeasurement>,
}
#[tauri::command]
pub async fn calibrate_frame_photometry(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: CalibratePhotometryRequest,
) -> Result<CalibratePhotometryResult, String> {
    let frame_index = request.frame_index;
    let calibrated = solve_frame_photometry(&app, &state, frame_index, request.band).await?;
    let (exposure, frame_path) = {
        let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
        let frame = frames.get(frame_index)?;
        (frame.metadata.exposure.unwrap_or(0.0), frame.path.clone())
    };
    let solution = calibrated.solution.clone();
    let catalog_sha256 = calibrated.catalog_sha256.clone();
    let mut session_measurements = state.measurements.lock().map_err(|e| e.to_string())?;
    for m in session_measurements
        .iter_mut()
        .filter(|m| m.frame_index == frame_index && m.frame_path == frame_path && m.flux > 0.0)
    {
        apply_photometric_solution(m, exposure, &calibrated);
    }
    let measurements = session_measurements
        .iter()
        .filter(|m| m.frame_index == frame_index && m.frame_path == frame_path)
        .cloned()
        .collect();
    Ok(CalibratePhotometryResult {
        matched_reference_stars: solution.reference_stars,
        solution,
        catalog_sha256,
        measurements,
    })
}

async fn solve_frame_photometry(
    app: &tauri::AppHandle,
    state: &AppState,
    frame_index: usize,
    requested_band: Option<String>,
) -> Result<FramePhotometry, String> {
    if let Some(cached) = state
        .frame_analyses
        .lock()
        .map_err(|e| e.to_string())?
        .get(frame_index)
        .and_then(|analysis| analysis.photometry.clone())
    {
        return Ok(cached);
    }
    let settings = crate::storage::load_config(app)?;
    let (wcs, detection, width, height, exposure) = {
        let a = state.frame_analyses.lock().map_err(|e| e.to_string())?;
        let analysis = a.get(frame_index).ok_or("Invalid frame index")?;
        let solution = analysis
            .solution
            .as_ref()
            .ok_or("Frame has no WCS solution")?;
        if solution.status != crate::astrometry::quality::ReductionStatus::Accepted {
            return Err("Photometric calibration requires an accepted WCS".into());
        }
        let wcs = solution.wcs.clone().ok_or("Accepted solution has no WCS")?;
        let detection = analysis
            .detection
            .clone()
            .ok_or("Run source detection first")?;
        let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
        let f = frames.get(frame_index)?;
        (
            wcs,
            detection,
            f.width,
            f.height,
            f.metadata.exposure.unwrap_or(0.0),
        )
    };
    if exposure <= 0.0 {
        return Err("Photometric calibration requires positive exposure time".into());
    }
    let band = requested_band.unwrap_or_else(|| settings.photometry.reference_band.clone());
    if !crate::catalog::refcat2::REFCAT2_BAND_MAPPINGS
        .iter()
        .any(|mapping| mapping.key == band)
    {
        return Err(
            "REFCAT2 reference band must be one of G/g/r/i/z; configure it in 光度与报告".into(),
        );
    }
    let radius = 0.5 * f64::from(width).hypot(f64::from(height)) * wcs.pixel_scale() / 3600.0 * 1.1;
    let catalog = state
        .refcat2
        .query(crate::catalog::refcat2::Refcat2Query {
            ra_deg: wcs.crval1,
            dec_deg: wcs.crval2,
            radius_deg: radius.clamp(0.001, 2.0),
            max_rows: Some(50_000),
        })
        .await?;
    let mut fwhms: Vec<f64> = detection
        .stars
        .iter()
        .filter(|s| !s.saturated && s.flags == 0 && s.fwhm > 0.5)
        .map(|s| s.fwhm)
        .collect();
    fwhms.sort_by(f64::total_cmp);
    let fwhm = fwhms.get(fwhms.len() / 2).copied().unwrap_or(3.0);
    let isolation_limit = (2.0 * fwhm * wcs.pixel_scale()).max(5.0);
    let (pixels, bkg) = {
        let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
        let f = frames.get(frame_index)?;
        let reducer = SepReducer;
        let b = reducer
            .background(&f.pixels, width, height, BackgroundConfig::default())
            .map_err(|e| e.to_string())?;
        (f.pixels.clone(), b)
    };
    let reducer = SepReducer;
    let catalog_total = catalog.stars.len();
    let mut quality_accepted = 0usize;
    let mut position_matched = 0usize;
    let mut aperture_accepted = 0usize;
    let mut samples = Vec::new();
    let aperture_radius = if settings.photometry.aperture_mode == "fixed" {
        settings.photometry.fixed_aperture_radius_px
    } else {
        settings.photometry.aperture_fwhm_multiplier * fwhm
    };
    let annulus_inner = if settings.photometry.aperture_mode == "fixed" {
        aperture_radius + settings.photometry.aperture_gap_px
    } else {
        settings.photometry.sky_annulus_inner_fwhm * fwhm
    };
    let annulus_outer = if settings.photometry.aperture_mode == "fixed" {
        annulus_inner + aperture_radius.max(2.0)
    } else {
        settings.photometry.sky_annulus_outer_fwhm * fwhm
    };
    for star in &catalog.stars {
        let (cat_mag, cat_err) = crate::catalog::refcat2::band_values(star, &band)
            .ok_or("unsupported REFCAT2 reference band")?;
        let Some(cat_mag) = cat_mag.filter(|value| value.is_finite() && *value > 0.0) else {
            continue;
        };
        if !cat_err.is_some_and(|value| {
            value.is_finite()
                && value >= 0.0
                && value <= settings.photometry.maximum_catalog_error_mag
        }) || !crate::catalog::refcat2::usable_dupvar(star.duplicate_variable)
            || star.isolation_1mag_arcsec.unwrap_or(0.0) < isolation_limit
        {
            continue;
        }
        quality_accepted += 1;
        let (px, py) = wcs.sky_to_pixel(star.ra_deg, star.dec_deg);
        let Some(src) = detection
            .stars
            .iter()
            .filter(|s| !s.saturated && s.flags == 0 && s.snr.unwrap_or(0.0) >= 20.0)
            .min_by(|a, b| ((a.x - px).hypot(a.y - py)).total_cmp(&((b.x - px).hypot(b.y - py))))
        else {
            continue;
        };
        if (src.x - px).hypot(src.y - py) > 3.0 {
            continue;
        }
        position_matched += 1;
        let phot = reducer
            .aperture_photometry(
                &pixels,
                width,
                height,
                ApertureRequest {
                    x: src.x,
                    y: src.y,
                    noise_rms: bkg.global_rms,
                    config: ApertureConfig {
                        radius: aperture_radius,
                        annulus_inner,
                        annulus_outer,
                        ..ApertureConfig::default()
                    },
                },
            )
            .map_err(|e| e.to_string())?;
        if phot.flux <= 0.0 || phot.snr.unwrap_or(0.0) < 20.0 {
            continue;
        }
        aperture_accepted += 1;
        let inst = -2.5 * (phot.flux / exposure).log10();
        let color = star
            .g_mag
            .zip(star.r_mag)
            .map(|(g, r)| g - r)
            .unwrap_or(f64::NAN);
        samples.push((cat_mag, inst, color));
    }
    let minimum = settings.photometry.minimum_reference_stars as usize;
    if samples.len() < minimum {
        return Err(format!(
            "只有 {} 颗可用 REFCAT2 参考星，配置要求至少 {} 颗（目录返回 {}，星表质量通过 {}，位置匹配 {}，孔径/SNR 通过 {}）",
            samples.len(),
            minimum,
            catalog_total,
            quality_accepted,
            position_matched,
            aperture_accepted,
        ));
    }
    let solution = crate::catalog::refcat2::robust_fit_with_options(
        &samples,
        &band,
        settings.photometry.fit_color_term,
        minimum,
        settings.photometry.maximum_residual_mag,
    )?;
    if !solution.accepted {
        return Err(solution.reason.clone());
    }
    let calibrated = FramePhotometry {
        solution,
        catalog_sha256: catalog.response_sha256,
    };
    let mut analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
    let analysis = analyses.get_mut(frame_index).ok_or("Invalid frame index")?;
    analysis.photometry = Some(calibrated.clone());
    analysis.photometry_error = None;
    Ok(calibrated)
}

fn apply_photometric_solution(
    measurement: &mut TargetMeasurement,
    exposure: f64,
    calibrated: &FramePhotometry,
) {
    if measurement.flux <= 0.0 || exposure <= 0.0 {
        return;
    }
    let solution = &calibrated.solution;
    let inst = -2.5 * (measurement.flux / exposure).log10();
    measurement.magnitude = Some(inst + solution.zero_point);
    measurement.magnitude_error =
        Some((1.085736 * measurement.flux_error / measurement.flux).hypot(solution.rms_mag));
    measurement.band = Some(solution.band.clone());
    measurement.photometric_catalog = Some("ATLAS2".into());
    measurement.provenance["refcat2_sha256"] =
        serde_json::Value::String(calibrated.catalog_sha256.clone());
    measurement.provenance["photometric_color_term_applied_to_target"] =
        serde_json::Value::Bool(false);
}

#[tauri::command]
pub fn measure_aperture(
    state: State<AppState>,
    params: ApertureParams,
) -> Result<ApertureMeasurement, String> {
    let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
    let idx = state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    let data = frames.get(*idx)?;
    let reducer = SepReducer;
    let background = reducer
        .background(
            &data.pixels,
            data.width,
            data.height,
            BackgroundConfig::default(),
        )
        .map_err(|error| error.to_string())?;
    let defaults = ApertureConfig::default();
    let config = ApertureConfig {
        radius: params.radius.unwrap_or(defaults.radius),
        annulus_inner: params.annulus_inner.unwrap_or(defaults.annulus_inner),
        annulus_outer: params.annulus_outer.unwrap_or(defaults.annulus_outer),
        ..defaults
    };
    reducer
        .aperture_photometry(
            &data.pixels,
            data.width,
            data.height,
            ApertureRequest {
                x: params.x,
                y: params.y,
                noise_rms: background.global_rms,
                config,
            },
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn measure_target(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: MeasureTargetRequest,
) -> Result<TargetMeasurement, String> {
    let settings = crate::storage::load_config(&app)?;
    let detection = detect_frame_async(
        &state,
        request.frame_index,
        settings.reduction.detection_sigma as f32,
        settings.reduction.minimum_fwhm_px,
        settings.reduction.maximum_reference_stars as usize,
        settings.instrument.saturation_adu,
    )
    .await?;
    let source = detection
        .stars
        .iter()
        .filter_map(|s| {
            let d = (s.x - request.x).hypot(s.y - request.y);
            (d <= 8.0).then_some((d, s))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|x| x.1);
    let (x, y, fwhm, ellipticity, source_flags, saturated, xerr, yerr) = source
        .map(|s| {
            (
                s.x,
                s.y,
                Some(s.fwhm),
                Some(s.ellipticity),
                s.flags,
                s.saturated,
                s.x_error_px,
                s.y_error_px,
            )
        })
        .unwrap_or((request.x, request.y, None, None, 0, false, None, None));
    let mut fwhms: Vec<f64> = detection
        .stars
        .iter()
        .filter(|s| !s.saturated && s.flags == 0 && s.fwhm.is_finite() && s.fwhm > 0.5)
        .map(|s| s.fwhm)
        .collect();
    fwhms.sort_by(f64::total_cmp);
    let frame_fwhm = fwhms.get(fwhms.len() / 2).copied().unwrap_or(3.0);
    let aperture_radius = if settings.photometry.aperture_mode == "fixed" {
        settings.photometry.fixed_aperture_radius_px
    } else {
        settings.photometry.aperture_fwhm_multiplier * frame_fwhm
    };
    let annulus_inner = if settings.photometry.aperture_mode == "fixed" {
        aperture_radius + settings.photometry.aperture_gap_px
    } else {
        settings.photometry.sky_annulus_inner_fwhm * frame_fwhm
    };
    let annulus_outer = if settings.photometry.aperture_mode == "fixed" {
        annulus_inner + aperture_radius.max(2.0)
    } else {
        settings.photometry.sky_annulus_outer_fwhm * frame_fwhm
    };
    let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
    let frame = frames.get(request.frame_index)?;
    let reducer = SepReducer;
    let bkg = reducer
        .background(
            &frame.pixels,
            frame.width,
            frame.height,
            BackgroundConfig::default(),
        )
        .map_err(|e| e.to_string())?;
    let aperture = reducer
        .aperture_photometry(
            &frame.pixels,
            frame.width,
            frame.height,
            ApertureRequest {
                x,
                y,
                noise_rms: bkg.global_rms,
                config: ApertureConfig {
                    radius: aperture_radius,
                    annulus_inner,
                    annulus_outer,
                    ..ApertureConfig::default()
                },
            },
        )
        .map_err(|e| e.to_string())?;
    let analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
    let analysis = analyses.get(request.frame_index);
    let solution = analysis.and_then(|a| a.solution.as_ref());
    let photometry = analysis.and_then(|a| a.photometry.clone());
    let wcs = solution.and_then(|s| s.wcs.as_ref());
    let sky = wcs.map(|w| w.pixel_to_sky(x, y));
    let scale = wcs.map(|w| w.pixel_scale());
    let wcs_accepted =
        solution.is_some_and(|s| s.status == crate::astrometry::quality::ReductionStatus::Accepted);
    let mut flags = Vec::new();
    if saturated {
        flags.push("saturated".into());
    }
    if source_flags != 0 || aperture.flags != 0 {
        flags.push("photometry_flags".into());
    }
    if x < 4.0 * frame_fwhm
        || y < 4.0 * frame_fwhm
        || x > frame.width as f64 - 4.0 * frame_fwhm
        || y > frame.height as f64 - 4.0 * frame_fwhm
    {
        flags.push("edge".into());
    }
    if ellipticity.is_some_and(|e| e > 0.45) {
        flags.push("trailed_or_elongated".into());
    }
    if !wcs_accepted {
        flags.push("wcs_not_accepted".into());
    }
    let midpoint_jd = frame.metadata.observation_midpoint_jd;
    let midpoint_utc = frame.metadata.observation_midpoint_utc.clone();
    let mut value = TargetMeasurement {
        id: uuid::Uuid::new_v4().to_string(),
        frame_index: request.frame_index,
        frame_path: frame.path.clone(),
        wcs_run_id: solution.and_then(|s| s.run_id.clone()),
        midpoint_utc,
        midpoint_jd,
        x,
        y,
        ra_deg: sky.map(|v| v.0),
        dec_deg: sky.map(|v| v.1),
        ra_uncertainty_arcsec: xerr.zip(scale).map(|(a, b)| a * b),
        dec_uncertainty_arcsec: yerr.zip(scale).map(|(a, b)| a * b),
        flux: aperture.flux,
        flux_error: aperture.flux_error,
        snr: aperture.snr,
        fwhm_px: fwhm,
        ellipticity,
        aperture_radius_px: aperture.aperture_radius,
        flags,
        magnitude: None,
        magnitude_error: None,
        band: None,
        photometric_catalog: None,
        designation: String::new(),
        match_status: MatchStatus::Unmatched,
        stale: false,
        stale_reason: None,
        provenance: serde_json::json!({"centroid":if source.is_some(){"SEP Gaussian-window centroid"}else{"user click"},"photometry":"SEP aperture","frame_median_fwhm_px":frame_fwhm}),
    };
    if let Some(calibrated) = photometry {
        apply_photometric_solution(
            &mut value,
            frame.metadata.exposure.unwrap_or(0.0),
            &calibrated,
        )
    }
    drop(analyses);
    drop(frames);
    state
        .pending_measurements
        .lock()
        .map_err(|e| e.to_string())?
        .insert(value.id.clone(), value.clone());
    Ok(value)
}
fn midpoint_adjustment_seconds(
    exposure: f64,
    settings: &crate::storage::TimeConfig,
    header_reference: Option<&str>,
) -> f64 {
    let reference = match header_reference {
        Some("average") => 0.0,
        Some("begin") => exposure / 2.0,
        _ => match settings.date_obs_reference.as_str() {
            "midpoint" => 0.0,
            "end" => -exposure / 2.0,
            _ => exposure / 2.0,
        },
    };
    reference + settings.utc_offset_hours * 3600.0 + settings.shutter_delay_seconds
}
fn configured_midpoint_jd(
    value: &str,
    exposure: f64,
    settings: &crate::storage::TimeConfig,
    header_reference: Option<&str>,
) -> Option<f64> {
    let base = AstroTime::from_fits_utc(value).ok()?.julian_date();
    Some(base + midpoint_adjustment_seconds(exposure, settings, header_reference) / 86_400.0)
}
fn midpoint_rfc3339(
    value: &str,
    exposure: f64,
    settings: &crate::storage::TimeConfig,
    header_reference: Option<&str>,
) -> Option<String> {
    use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
    let v = if value.ends_with('Z') {
        value.to_string()
    } else {
        format!("{value}Z")
    };
    let midpoint = OffsetDateTime::parse(&v, &Rfc3339)
        .ok()?
        .checked_add(Duration::seconds_f64(midpoint_adjustment_seconds(
            exposure,
            settings,
            header_reference,
        )))?;
    let precision = settings.precision_seconds.clamp(1e-9, 1.0);
    let quantum_ns = (precision * 1e9).round().max(1.0) as i128;
    let timestamp_ns = midpoint.unix_timestamp_nanos();
    let rounded_ns = (timestamp_ns + quantum_ns / 2).div_euclid(quantum_ns) * quantum_ns;
    let rounded = OffsetDateTime::from_unix_timestamp_nanos(rounded_ns).ok()?;
    let digits: usize = (0usize..=9)
        .find(|digits| {
            let scaled = precision * 10f64.powi(*digits as i32);
            (scaled - scaled.round()).abs() < 1e-9
        })
        .unwrap_or(9);
    let mut result = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        rounded.year(),
        u8::from(rounded.month()),
        rounded.day(),
        rounded.hour(),
        rounded.minute(),
        rounded.second()
    );
    if digits > 0 {
        let divisor = 10u32.pow(9 - digits as u32);
        let fraction = rounded.nanosecond() / divisor;
        result.push_str(&format!(".{fraction:0digits$}"));
    }
    result.push('Z');
    Some(result)
}

#[tauri::command]
pub fn list_target_measurements(state: State<AppState>) -> Result<Vec<TargetMeasurement>, String> {
    Ok(state
        .measurements
        .lock()
        .map_err(|e| e.to_string())?
        .clone())
}
#[tauri::command]
pub fn delete_target_measurement(state: State<AppState>, id: String) -> Result<(), String> {
    state
        .measurements
        .lock()
        .map_err(|e| e.to_string())?
        .retain(|measurement| measurement.id != id);
    Ok(())
}
#[tauri::command]
pub fn confirm_target_measurement(
    state: State<AppState>,
    id: String,
    designation: String,
) -> Result<TargetMeasurement, String> {
    let designation = normalize_tracklet_designation(&designation)?;
    let mut pending = state
        .pending_measurements
        .lock()
        .map_err(|e| e.to_string())?;
    let mut measurement = pending
        .remove(&id)
        .ok_or("待确认的可疑目标不存在或已经失效")?;
    measurement.designation = designation;
    drop(pending);
    state
        .measurements
        .lock()
        .map_err(|e| e.to_string())?
        .push(measurement.clone());
    Ok(measurement)
}
#[tauri::command]
pub fn discard_target_measurement(state: State<AppState>, id: String) -> Result<(), String> {
    state
        .pending_measurements
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&id);
    Ok(())
}
#[tauri::command]
pub fn rename_target_measurement(
    state: State<AppState>,
    id: String,
    name: String,
) -> Result<TargetMeasurement, String> {
    let mut measurements = state.measurements.lock().map_err(|e| e.to_string())?;
    let measurement = measurements
        .iter_mut()
        .find(|measurement| measurement.id == id)
        .ok_or("Suspicious target not found")?;
    measurement.designation = normalize_tracklet_designation(&name)?;
    Ok(measurement.clone())
}

fn mpc_root(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(crate::storage::initialize(app)?.mpcorb_dir)
}
#[tauri::command]
pub async fn update_mpcorb(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<MpcorbManifest, String> {
    let manifest = crate::mpcorb::download_and_activate(&mpc_root(&app)?).await?;
    *state.mpcorb_cache.lock().map_err(|e| e.to_string())? = None;
    Ok(manifest)
}
/// Import a local MPCORB.DAT.gz as an alternative to `update_mpcorb` (auto-download).
#[tauri::command]
pub async fn import_mpcorb(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_path: String,
) -> Result<MpcorbManifest, String> {
    let root = mpc_root(&app)?;
    let source = std::path::PathBuf::from(&source_path);
    if !source.is_file() {
        return Err(format!("{source_path} is not a valid file"));
    }
    // Accept both .gz and .GZ — Windows/Linux downloads often keep the
    // uppercase extension from the MPC website (MPCORB.DAT.GZ).
    if !source
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
    {
        return Err("please select a .gz compressed MPCORB catalog (e.g. MPCORB.DAT.gz)".into());
    }
    // Reuse the same activation pipeline as auto-download, then drop the cached orbits
    // so the next known-object query picks up the imported database. Parsing the full
    // catalog takes minutes, so run it on the blocking pool like `update_mpcorb`.
    let manifest =
        tokio::task::spawn_blocking(move || crate::mpcorb::activate_local_gz(&root, &source))
            .await
            .map_err(|e| format!("MPCORB import worker failed: {e}"))?
            .map_err(|e| format!("MPCORB import failed: {e}"))?;
    *state.mpcorb_cache.lock().map_err(|e| e.to_string())? = None;
    Ok(manifest)
}
#[tauri::command]
pub fn get_mpcorb_status(app: tauri::AppHandle) -> Result<Option<MpcorbManifest>, String> {
    match crate::mpcorb::load_active_manifest(&mpc_root(&app)?) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.contains("cannot find") || e.contains("os error 2") => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnownObjectSearch {
    pub jd_utc: f64,
    pub center_ra_deg: f64,
    pub center_dec_deg: f64,
    pub radius_deg: f64,
    pub station: Option<Observatory>,
    pub max_results: Option<usize>,
}
#[derive(Debug, Serialize)]
pub struct KnownObjectResult {
    pub database: MpcorbManifest,
    pub objects: Vec<EphemerisPoint>,
    pub source: &'static str,
    pub warnings: Vec<String>,
}
#[tauri::command]
pub async fn search_known_objects(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: KnownObjectSearch,
) -> Result<KnownObjectResult, String> {
    if !(0.0..=10.0).contains(&request.radius_deg) {
        return Err("radius must be in [0,10] degrees".into());
    }
    let (m, orbits) = active_orbits(&state, mpc_root(&app)?).await?;
    let settings = crate::storage::load_config(&app)?;
    let online_request = request.clone();
    let propagation = PropagationContext::new(request.jd_utc, request.station)?;
    let local_objects = tokio::task::spawn_blocking(move || {
        let mut objects: Vec<_> = orbits
            .par_iter()
            .filter_map(|orbit| propagate_with_context(orbit, &propagation).ok())
            .filter(|p| {
                angular_distance_arcsec(
                    request.center_ra_deg,
                    request.center_dec_deg,
                    p.ra_deg,
                    p.dec_deg,
                ) <= request.radius_deg * 3600.0
            })
            .collect();
        objects.sort_by(|a, b| {
            a.predicted_mag
                .unwrap_or(99.)
                .total_cmp(&b.predicted_mag.unwrap_or(99.))
        });
        objects.truncate(request.max_results.unwrap_or(1000).min(5000));
        objects
    })
    .await
    .map_err(|e| format!("known-object worker failed: {e}"))?;
    let mut source = "mpcorb_local";
    let mut warnings = Vec::new();
    let objects = if settings.data.jpl_mode == "offline" {
        warnings.push("JPL 离线模式已启用；当前显示本地 MPCORB 预测".to_string());
        local_objects
    } else if let Some(station) = online_request.station {
        match state
            .jpl
            .identify_second_pass(
                online_request.jd_utc,
                online_request.center_ra_deg,
                online_request.center_dec_deg,
                online_request.radius_deg,
                station,
                std::time::Duration::from_secs(settings.data.jpl_timeout_seconds),
            )
            .await
        {
            Ok(mut precise) => {
                source = "jpl_second_pass";
                precise.truncate(online_request.max_results.unwrap_or(1000).min(5000));
                precise
            }
            Err(error) => {
                log::warn!("JPL second-pass unavailable; retaining local predictions: {error}");
                warnings.push(format!("JPL 在线复核失败，当前显示本地预测：{error}"));
                local_objects
            }
        }
    } else {
        local_objects
    };
    Ok(KnownObjectResult {
        database: m,
        objects,
        source,
        warnings,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnownObjectFrameSearch {
    pub frame_index: usize,
    pub jd_utc: f64,
    pub center_ra_deg: f64,
    pub center_dec_deg: f64,
    pub radius_deg: f64,
}
#[derive(Debug, Clone, Deserialize)]
pub struct KnownObjectBatchSearch {
    pub frames: Vec<KnownObjectFrameSearch>,
    pub station: Option<Observatory>,
    pub max_results_per_frame: Option<usize>,
}
#[derive(Debug, Serialize)]
pub struct KnownObjectFrameResult {
    pub frame_index: usize,
    pub objects: Vec<EphemerisPoint>,
    pub source: &'static str,
    pub warnings: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct KnownObjectBatchResult {
    pub database: MpcorbManifest,
    pub frames: Vec<KnownObjectFrameResult>,
}
#[tauri::command]
pub async fn search_known_objects_batch(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: KnownObjectBatchSearch,
) -> Result<KnownObjectBatchResult, String> {
    if request.frames.is_empty() {
        return Err("known-object batch requires at least one frame".into());
    }
    if request
        .frames
        .iter()
        .any(|f| !(0.0..=10.0).contains(&f.radius_deg))
    {
        return Err("all frame radii must be in [0,10] degrees".into());
    }
    let (manifest, orbits) = active_orbits(&state, mpc_root(&app)?).await?;
    let frames = request.frames;
    let local_frames = frames.clone();
    let station = request.station;
    let limit = request.max_results_per_frame.unwrap_or(1000).min(5000);
    let start = local_frames
        .iter()
        .map(|frame| frame.jd_utc)
        .fold(f64::INFINITY, f64::min);
    let end = local_frames
        .iter()
        .map(|frame| frame.jd_utc)
        .fold(f64::NEG_INFINITY, f64::max);
    let start_context = PropagationContext::new(start, station)?;
    let end_context = PropagationContext::new(end, station)?;
    let frame_contexts = local_frames
        .iter()
        .map(|frame| PropagationContext::new(frame.jd_utc, station))
        .collect::<Result<Vec<_>, _>>()?;
    let mut grouped = tokio::task::spawn_blocking(move || {
        let span_minutes = (end - start).abs() * 1440.0;
        let hits: Vec<(usize, EphemerisPoint)> = orbits
            .par_iter()
            .filter_map(|orbit| {
                let p0 = propagate_with_context(orbit, &start_context).ok()?;
                let p1 = propagate_with_context(orbit, &end_context).ok()?;
                let padding_deg = (p0.angular_speed_arcsec_min.max(p1.angular_speed_arcsec_min)
                    * span_minutes
                    / 3600.0)
                    .min(30.0);
                let may_cross = local_frames.iter().any(|f| {
                    let limit = (f.radius_deg + padding_deg) * 3600.0;
                    angular_distance_arcsec(
                        f.center_ra_deg,
                        f.center_dec_deg,
                        p0.ra_deg,
                        p0.dec_deg,
                    ) <= limit
                        || angular_distance_arcsec(
                            f.center_ra_deg,
                            f.center_dec_deg,
                            p1.ra_deg,
                            p1.dec_deg,
                        ) <= limit
                });
                if !may_cross {
                    return None;
                }
                let exact: Vec<_> = local_frames
                    .iter()
                    .zip(&frame_contexts)
                    .filter_map(|(f, context)| {
                        let p = propagate_with_context(orbit, context).ok()?;
                        (angular_distance_arcsec(
                            f.center_ra_deg,
                            f.center_dec_deg,
                            p.ra_deg,
                            p.dec_deg,
                        ) <= f.radius_deg * 3600.0)
                            .then_some((f.frame_index, p))
                    })
                    .collect();
                (!exact.is_empty()).then_some(exact)
            })
            .flatten()
            .collect();
        let mut map: HashMap<usize, Vec<EphemerisPoint>> = HashMap::new();
        for (index, point) in hits {
            map.entry(index).or_default().push(point)
        }
        local_frames
            .into_iter()
            .map(|f| {
                let mut objects = map.remove(&f.frame_index).unwrap_or_default();
                objects.sort_by(|a, b| {
                    a.predicted_mag
                        .unwrap_or(99.0)
                        .total_cmp(&b.predicted_mag.unwrap_or(99.0))
                });
                objects.truncate(limit);
                KnownObjectFrameResult {
                    frame_index: f.frame_index,
                    objects,
                    source: "mpcorb_local",
                    warnings: Vec::new(),
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("known-object batch worker failed: {e}"))?;
    grouped.sort_by_key(|f| f.frame_index);
    let settings = crate::storage::load_config(&app)?;
    if settings.data.jpl_mode == "offline" {
        for frame in &mut grouped {
            frame
                .warnings
                .push("JPL 离线模式已启用；当前显示本地 MPCORB 预测".to_string());
        }
    } else if let Some(station) = station {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(3));
        let mut tasks = tokio::task::JoinSet::new();
        for frame in frames {
            let semaphore = semaphore.clone();
            let jpl = state.jpl.clone();
            let timeout = std::time::Duration::from_secs(settings.data.jpl_timeout_seconds);
            tasks.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|error| error.to_string())?;
                let result = jpl
                    .identify_second_pass(
                        frame.jd_utc,
                        frame.center_ra_deg,
                        frame.center_dec_deg,
                        frame.radius_deg,
                        station,
                        timeout,
                    )
                    .await;
                Ok::<_, String>((frame.frame_index, result))
            });
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok((frame_index, Ok(mut precise)))) => {
                    precise.truncate(limit);
                    if let Some(frame) = grouped
                        .iter_mut()
                        .find(|frame| frame.frame_index == frame_index)
                    {
                        frame.objects = precise;
                        frame.source = "jpl_second_pass";
                    }
                }
                Ok(Ok((frame_index, Err(error)))) => {
                    log::warn!("frame {frame_index}: JPL second-pass unavailable; retaining local predictions: {error}");
                    if let Some(frame) = grouped
                        .iter_mut()
                        .find(|frame| frame.frame_index == frame_index)
                    {
                        frame
                            .warnings
                            .push(format!("JPL 在线复核失败，当前显示本地预测：{error}"));
                    }
                }
                Ok(Err(error)) => log::warn!("JPL refinement task failed: {error}"),
                Err(error) => log::warn!("JPL refinement task join failed: {error}"),
            }
        }
    }
    Ok(KnownObjectBatchResult {
        database: manifest,
        frames: grouped,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackletMatchRequest {
    pub points: Vec<TrackletPoint>,
    pub station: Option<Observatory>,
}
#[tauri::command]
pub async fn match_tracklet(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: TrackletMatchRequest,
) -> Result<TrackletMatchResult, String> {
    if request.points.len() < 2 {
        return Err("tracklet matching requires at least two measured frames".into());
    }
    let (m, orbits) = active_orbits(&state, mpc_root(&app)?).await?;
    tokio::task::spawn_blocking(move || {
        let mut candidates: Vec<CandidateMatch> = orbits
            .par_iter()
            .filter_map(|o| {
                let mut residuals = Vec::with_capacity(request.points.len());
                let mut speeds = Vec::new();
                for point in &request.points {
                    let p = propagate(o, point.jd_utc, request.station).ok()?;
                    let r =
                        angular_distance_arcsec(point.ra_deg, point.dec_deg, p.ra_deg, p.dec_deg);
                    if r > 30.0 {
                        return None;
                    }
                    residuals.push(r);
                    speeds.push(p.angular_speed_arcsec_min);
                }
                Some(CandidateMatch {
                    designation: o.designation.clone(),
                    max_residual_arcsec: residuals.iter().copied().fold(0., f64::max),
                    residuals_arcsec: residuals,
                    mean_speed_arcsec_min: speeds.iter().sum::<f64>() / speeds.len() as f64,
                })
            })
            .collect();
        candidates.sort_by(|a, b| a.max_residual_arcsec.total_cmp(&b.max_residual_arcsec));
        candidates.truncate(20);
        let probable: Vec<_> = candidates
            .iter()
            .filter(|c| c.max_residual_arcsec <= 5.0)
            .collect();
        let (status, reason) = if probable.len() == 1 && !m.stale() {
            (
                MatchStatus::Ambiguous,
                "唯一的本地二体传播候选在全部帧的 O-C 均不超过 5 arcsec；需 JPL second-pass 或 MPC 复核".into(),
            )
        } else if candidates.is_empty() {
            (
                MatchStatus::NoLocalMatch,
                if m.too_stale_for_no_match() {
                    "本地数据超过 7 天，不能据此解释未知目标"
                } else {
                    "no local match；这不等于新发现"
                }
                .into(),
            )
        } else {
            (
                MatchStatus::Ambiguous,
                "候选不唯一、数据过期或传播为 approximate".into(),
            )
        };
        Ok(TrackletMatchResult {
            status,
            candidates,
            reason,
            database_sha256: m.sha256.clone(),
            database_stale: m.stale(),
        })
    })
    .await
    .map_err(|e| format!("tracklet worker failed: {e}"))?
}

async fn active_orbits(
    state: &AppState,
    root: std::path::PathBuf,
) -> Result<(MpcorbManifest, Arc<Vec<crate::mpcorb::OrbitRecord>>), String> {
    if let Some(cached) = state
        .mpcorb_cache
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
    {
        return Ok(cached);
    }
    let (manifest, records) =
        tokio::task::spawn_blocking(move || crate::mpcorb::load_active(&root))
            .await
            .map_err(|e| format!("MPCORB index worker failed: {e}"))??;
    let value = (manifest, Arc::new(records));
    *state.mpcorb_cache.lock().map_err(|e| e.to_string())? = Some(value.clone());
    Ok(value)
}

#[tauri::command]
pub fn preview_ades(request: crate::ades::AdesRequest) -> Result<String, String> {
    crate::ades::render(&request).map_err(|e| e.join("；"))
}
#[derive(Debug, Deserialize)]
pub struct ExportAdesRequest {
    pub destination: String,
    pub report: crate::ades::AdesRequest,
}
#[tauri::command]
pub fn export_ades(request: ExportAdesRequest) -> Result<String, String> {
    let content = crate::ades::render(&request.report).map_err(|e| e.join("；"))?;
    let path = std::path::Path::new(&request.destination);
    if path.extension().and_then(|s| s.to_str()) != Some("psv") {
        return Err("ADES export destination must use .psv extension".into());
    }
    std::fs::write(path, &content).map_err(|e| e.to_string())?;
    Ok(uuid::Uuid::new_v4().to_string())
}

#[tauri::command]
pub fn preview_report(request: crate::report::ReportRequest) -> Result<String, String> {
    crate::report::writer(request.format)
        .render(&request.context, &request.observations)
        .map_err(|e| e.join("；"))
}
#[derive(Debug, Deserialize)]
pub struct ExportReportRequest {
    pub destination: String,
    pub request: crate::report::ReportRequest,
}
#[tauri::command]
pub fn export_report(payload: ExportReportRequest) -> Result<String, String> {
    let writer = crate::report::writer(payload.request.format);
    let content = writer
        .render(&payload.request.context, &payload.request.observations)
        .map_err(|e| e.join("；"))?;
    let path = std::path::Path::new(&payload.destination);
    if path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case(writer.extension()))
        != Some(true)
    {
        return Err(format!("该报告格式要求 .{} 扩展名", writer.extension()));
    }
    std::fs::write(path, &content).map_err(|e| e.to_string())?;
    Ok(uuid::Uuid::new_v4().to_string())
}

#[tauri::command]
pub fn get_app_config(app: tauri::AppHandle) -> Result<crate::storage::AppConfig, String> {
    crate::storage::load_config(&app)
}
#[tauri::command]
pub fn save_app_config(
    app: tauri::AppHandle,
    state: State<AppState>,
    config: crate::storage::AppConfig,
) -> Result<ConfigSaveResult, String> {
    let previous = crate::storage::load_config(&app)?;
    let previous_science = serde_json::json!({
        "instrument": previous.instrument,
        "time": previous.time,
        "reduction": previous.reduction,
        "photometry": previous.photometry,
    });
    let next_science = serde_json::json!({
        "instrument": &config.instrument,
        "time": &config.time,
        "reduction": &config.reduction,
        "photometry": &config.photometry,
    });
    crate::storage::save_config(&app, &config)?;
    let scientific_cache_invalidated = previous_science != next_science;
    if scientific_cache_invalidated {
        for analysis in state
            .frame_analyses
            .lock()
            .map_err(|error| error.to_string())?
            .iter_mut()
        {
            *analysis = FrameAnalysis::default();
        }
        for index in 0..state
            .loaded_frames
            .lock()
            .map_err(|error| error.to_string())?
            .len()
        {
            mark_frame_measurements_stale(&state, index, "科学设置已变化，需要重新归算")?;
        }
        for run in state
            .reduction_runs
            .lock()
            .map_err(|error| error.to_string())?
            .values_mut()
        {
            run["active"] = serde_json::Value::Bool(false);
        }
    }
    Ok(ConfigSaveResult {
        scientific_cache_invalidated,
    })
}

#[derive(Debug, Serialize)]
pub struct ConfigSaveResult {
    pub scientific_cache_invalidated: bool,
}

#[tauri::command]
pub fn load_app_config_file(path: String) -> Result<crate::storage::AppConfig, String> {
    crate::storage::load_config_file(std::path::Path::new(&path))
}

#[tauri::command]
pub fn save_app_config_file(path: String, config: crate::storage::AppConfig) -> Result<(), String> {
    crate::storage::save_config_file(std::path::Path::new(&path), &config)
}
#[tauri::command]
pub fn get_storage_layout(app: tauri::AppHandle) -> Result<crate::storage::StorageLayout, String> {
    crate::storage::initialize(&app)
}

#[tauri::command]
pub async fn plate_solve(
    state: State<'_, AppState>,
    params: SolveParams,
) -> Result<platesolve::PlateSolveResult, String> {
    let idx = state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?
        .to_owned();
    solve_frame(&state, idx, params).await
}

async fn solve_frame(
    state: &AppState,
    idx: usize,
    params: SolveParams,
) -> Result<platesolve::PlateSolveResult, String> {
    if params
        .catalog_bright_limit_mag
        .zip(params.catalog_faint_limit_mag)
        .is_some_and(|(bright, faint)| bright > faint)
    {
        return Err("Gaia bright magnitude limit must not exceed the faint limit".to_string());
    }
    let (
        header_ra,
        header_dec,
        header_scale,
        header_rotation,
        header_parity,
        observation_jd,
        estimated_radius,
        image_width,
        image_height,
        upstream_seed,
    ) = {
        let mut frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
        let data = frames.get(idx)?;
        let observation_jd = data.metadata.observation_midpoint_jd;
        let upstream_seed =
            data.metadata
                .upstream_wcs
                .as_ref()
                .map(|hint| crate::astrometry::wcs::Wcs {
                    crpix1: hint.crpix1 - 1.0,
                    crpix2: hint.crpix2 - 1.0,
                    crval1: hint.crval1,
                    crval2: hint.crval2,
                    cd1_1: hint.cd1_1,
                    cd1_2: hint.cd1_2,
                    cd2_1: hint.cd2_1,
                    cd2_2: hint.cd2_2,
                    image_width: data.width,
                    image_height: data.height,
                    sip: None,
                });
        (
            data.metadata.ra,
            data.metadata.dec,
            data.metadata.pixel_scale_arcsec,
            data.metadata.rotation_deg,
            data.metadata.parity_flipped,
            observation_jd,
            estimate_cone_radius(&data),
            data.width,
            data.height,
            upstream_seed,
        )
    };
    let Some(ra_deg) = params.center_ra_deg.or(header_ra) else {
        return Ok(platesolve::missing_hint(
            "归算需要中心 RA：FITS Header 中没有可解析的 RA，请在设备/归算参数中填写。",
        ));
    };
    let Some(dec_deg) = params.center_dec_deg.or(header_dec) else {
        return Ok(platesolve::missing_hint(
            "归算需要中心 Dec：FITS Header 中没有可解析的 Dec，请在设备/归算参数中填写。",
        ));
    };
    let pixel_scale_arcsec = params.pixel_scale_arcsec.or(header_scale);
    let rotation_deg = params.rotation_deg.or(header_rotation);
    let parity_flipped = params.parity_flipped.or(header_parity);
    let scale_radius = pixel_scale_arcsec.map(|scale| {
        let diagonal = f64::from(image_width).hypot(f64::from(image_height));
        (0.5 * diagonal * scale / 3_600.0 * 1.15).clamp(0.01, 2.0)
    });
    let footprint_radius = upstream_seed.as_ref().map(wcs_footprint_radius);
    let radius_deg = params
        .radius_deg
        .or(footprint_radius)
        .or(scale_radius)
        .or(estimated_radius)
        .unwrap_or(0.5);
    let detection = state
        .frame_analyses
        .lock()
        .map_err(|error| error.to_string())?
        .get(idx)
        .and_then(|analysis| analysis.detection.clone());
    let Some(detection) = detection else {
        return Ok(platesolve::missing_hint("归算前必须先完成本帧星点提取。"));
    };
    let catalog = state
        .vizier
        .query_gaia(
            GaiaQuery {
                ra_deg,
                dec_deg,
                radius_deg,
                observation_jd,
                max_rows: Some(10_000),
            },
            CancellationToken::new(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let num_catalog = catalog.sources.len() as u32;
    let available = detection.astrometry_stars.len();
    let reference_limits = reference_count_attempts(
        available,
        params.maximum_reference_stars.unwrap_or(200),
        params.plate_model.unwrap_or_default(),
    );
    let seed = initial_wcs_seed(
        &params,
        upstream_seed,
        ra_deg,
        dec_deg,
        pixel_scale_arcsec,
        rotation_deg,
        parity_flipped,
        image_width,
        image_height,
    );
    let match_params = params.clone();
    let catalog_sources = catalog.sources.clone();
    let accept_review = params.accept_review.unwrap_or(false);
    let mut result = tokio::task::spawn_blocking(move || {
        compute_plate_solution(
            match_params,
            detection,
            catalog_sources,
            seed,
            reference_limits,
            num_catalog,
            ra_deg,
            dec_deg,
            image_width,
            image_height,
            pixel_scale_arcsec,
            rotation_deg,
            parity_flipped,
        )
    })
    .await
    .map_err(|e| format!("plate-matching worker failed: {e}"))?;
    if accept_review {
        result.confirm_review();
    }
    persist_reduction_result(state, idx, &params, &catalog, &mut result)?;
    let mut analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
    let analysis = analyses
        .get_mut(idx)
        .ok_or("Invalid frame analysis index")?;
    analysis.catalog = Some(catalog);
    analysis.solution = Some(result.clone());
    analysis.photometry = None;
    analysis.photometry_error = None;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn compute_plate_solution(
    params: SolveParams,
    detection: DetectionResult,
    catalog_sources: Vec<crate::catalog::vizier::GaiaSource>,
    seed: Option<crate::astrometry::wcs::Wcs>,
    reference_limits: Vec<usize>,
    num_catalog: u32,
    ra_deg: f64,
    dec_deg: f64,
    image_width: u32,
    image_height: u32,
    pixel_scale_arcsec: Option<f64>,
    rotation_deg: Option<f64>,
    parity_flipped: Option<bool>,
) -> platesolve::PlateSolveResult {
    let mut best_result = None;
    let mut last_error = "not enough usable stars after quality filtering".to_string();
    let maximum_rms_arcsec = params.astrometric_residual_limit_arcsec.unwrap_or(3.0);
    if let Some(seed) = seed {
        for reference_limit in &reference_limits {
            log::debug!(
                "[sky-eye][matcher] seeded attempt: references={} G=[{:?}, {:?}]",
                reference_limit,
                params.catalog_bright_limit_mag,
                params.catalog_faint_limit_mag
            );
            match matcher::refine_from_wcs_seed(
                &detection.astrometry_stars,
                &catalog_sources,
                seed.clone(),
                seeded_match_config(
                    &params,
                    *reference_limit,
                    maximum_rms_arcsec,
                    catalog_sources.len(),
                ),
            ) {
                Ok(solution) => {
                    let candidate =
                        platesolve::solved(num_catalog, solution, &detection.astrometry_stars);
                    let accepted = candidate.success;
                    retain_better_candidate(&mut best_result, candidate);
                    if accepted {
                        break;
                    }
                }
                Err(error) => last_error = format!("seeded WCS refinement failed: {error}"),
            }
        }
    }
    for (attempt, reference_limit) in reference_limits.into_iter().enumerate() {
        if best_result.as_ref().is_some_and(|r| r.success) {
            break;
        }
        log::debug!(
            "[sky-eye][matcher] automatic attempt {}: references={} G=[{:?}, {:?}]",
            attempt + 1,
            reference_limit,
            params.catalog_bright_limit_mag,
            params.catalog_faint_limit_mag
        );
        match matcher::solve_near_field(
            &detection.astrometry_stars,
            &catalog_sources,
            ra_deg,
            dec_deg,
            image_width,
            image_height,
            automatic_match_config(
                &params,
                reference_limit,
                maximum_rms_arcsec,
                pixel_scale_arcsec,
                rotation_deg,
                parity_flipped,
            ),
        ) {
            Ok(solution) => {
                let candidate =
                    platesolve::solved(num_catalog, solution, &detection.astrometry_stars);
                let accepted = candidate.success;
                retain_better_candidate(&mut best_result, candidate);
                if accepted {
                    break;
                }
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    best_result.unwrap_or_else(|| platesolve::match_failed(num_catalog, last_error))
}

fn reference_count_attempts(
    available: usize,
    configured_maximum: usize,
    plate_model: matcher::PlateModel,
) -> Vec<usize> {
    let maximum = available.min(configured_maximum.clamp(8, 500));
    // Higher-order plate models need more validated references (36 for
    // quadratic, 60 for cubic), so widen the candidate pool rather than
    // lowering any acceptance gate when the requested order allows it.
    let mut tiers: Vec<usize> = if plate_model == matcher::PlateModel::Linear {
        vec![50, 90, 140, 200]
    } else {
        vec![50, 90, 140, 200, 280, 360, 500]
    };
    let mut attempts: Vec<_> = tiers
        .drain(..)
        .map(|limit| limit.min(maximum))
        .filter(|limit| *limit >= 4)
        .collect();
    attempts.dedup();
    attempts
}

fn seeded_match_config(
    params: &SolveParams,
    reference_limit: usize,
    maximum_rms_arcsec: f64,
    catalog_len: usize,
) -> matcher::MatchConfig {
    matcher::MatchConfig {
        max_image_sources: reference_limit,
        // A manual/seeded alignment already constrains the transform. Keep the
        // image list selective, but search a wider Gaia pool so saturated or
        // otherwise missing bright stars do not crowd the real counterparts out
        // of both equally-sized lists.
        max_catalog_sources: catalog_len.min(256),
        maximum_rms_arcsec,
        catalog_bright_limit_mag: params.catalog_bright_limit_mag,
        catalog_faint_limit_mag: params.catalog_faint_limit_mag,
        plate_model: params.plate_model.unwrap_or_default(),
        ..matcher::MatchConfig::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn automatic_match_config(
    params: &SolveParams,
    reference_limit: usize,
    maximum_rms_arcsec: f64,
    pixel_scale_arcsec: Option<f64>,
    rotation_deg: Option<f64>,
    parity_flipped: Option<bool>,
) -> matcher::MatchConfig {
    matcher::MatchConfig {
        max_image_sources: reference_limit,
        max_catalog_sources: reference_limit,
        maximum_rms_arcsec,
        pixel_scale_hint_arcsec: pixel_scale_arcsec,
        rotation_hint_deg: rotation_deg,
        parity_hint: parity_flipped,
        catalog_bright_limit_mag: params.catalog_bright_limit_mag,
        catalog_faint_limit_mag: params.catalog_faint_limit_mag,
        plate_model: params.plate_model.unwrap_or_default(),
        ..matcher::MatchConfig::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn initial_wcs_seed(
    params: &SolveParams,
    upstream_seed: Option<crate::astrometry::wcs::Wcs>,
    ra_deg: f64,
    dec_deg: f64,
    pixel_scale_arcsec: Option<f64>,
    rotation_deg: Option<f64>,
    parity_flipped: Option<bool>,
    image_width: u32,
    image_height: u32,
) -> Option<crate::astrometry::wcs::Wcs> {
    let has_explicit_geometry = params.center_ra_deg.is_some()
        || params.center_dec_deg.is_some()
        || params.pixel_scale_arcsec.is_some()
        || params.rotation_deg.is_some()
        || params.parity_flipped.is_some()
        || params.offset_x_px.is_some()
        || params.offset_y_px.is_some();
    if !has_explicit_geometry {
        if let Some(seed) = upstream_seed {
            return Some(seed);
        }
    }

    let scale = pixel_scale_arcsec? / 3_600.0;
    let angle = rotation_deg.unwrap_or(0.0).to_radians();
    let cosine = angle.cos();
    let sine = angle.sin();
    let (cd1_1, cd1_2, cd2_1, cd2_2) = if parity_flipped.unwrap_or(false) {
        (scale * cosine, scale * sine, scale * sine, -scale * cosine)
    } else {
        (scale * cosine, -scale * sine, scale * sine, scale * cosine)
    };
    Some(crate::astrometry::wcs::Wcs {
        crpix1: f64::from(image_width) * 0.5 + params.offset_x_px.unwrap_or(0.0),
        crpix2: f64::from(image_height) * 0.5 + params.offset_y_px.unwrap_or(0.0),
        crval1: ra_deg,
        crval2: dec_deg,
        cd1_1,
        cd1_2,
        cd2_1,
        cd2_2,
        image_width,
        image_height,
        sip: None,
    })
}

fn retain_better_candidate(
    best: &mut Option<platesolve::PlateSolveResult>,
    candidate: platesolve::PlateSolveResult,
) {
    if best
        .as_ref()
        .is_none_or(|current| candidate_is_better(&candidate, current))
    {
        *best = Some(candidate);
    }
}

fn candidate_is_better(
    candidate: &platesolve::PlateSolveResult,
    current: &platesolve::PlateSolveResult,
) -> bool {
    let status_rank = |result: &platesolve::PlateSolveResult| match result.status.as_str() {
        "accepted" => 2,
        "review_required" => 1,
        _ => 0,
    };
    let candidate_rank = status_rank(candidate);
    let current_rank = status_rank(current);
    if candidate_rank != current_rank {
        return candidate_rank > current_rank;
    }

    let candidate_quality = candidate.quality.as_ref();
    let current_quality = current.quality.as_ref();
    let candidate_matches = candidate_quality.map_or(0, |quality| quality.matched);
    let current_matches = current_quality.map_or(0, |quality| quality.matched);
    if candidate_matches != current_matches {
        return candidate_matches > current_matches;
    }

    candidate_quality.map_or(f64::INFINITY, |quality| quality.residual_rms_arcsec)
        < current_quality.map_or(f64::INFINITY, |quality| quality.residual_rms_arcsec)
}

fn persist_reduction_result(
    state: &AppState,
    frame_index: usize,
    params: &SolveParams,
    catalog: &GaiaQueryResult,
    result: &mut platesolve::PlateSolveResult,
) -> Result<(), String> {
    let (frame_path, provenance) = {
        let mut frames = state
            .loaded_frames
            .lock()
            .map_err(|error| error.to_string())?;
        let frame = frames.get(frame_index)?;
        let pca: Vec<_> = frame
            .metadata
            .panstarrs_pca
            .iter()
            .map(|coefficient| {
                serde_json::json!({
                    "axis": coefficient.axis,
                    "x_order": coefficient.x_order,
                    "y_order": coefficient.y_order,
                    "value": coefficient.value,
                })
            })
            .collect();
        (
            frame.path.clone(),
            serde_json::json!({
                "software": "SkyEye",
                "version": env!("CARGO_PKG_VERSION"),
                "backend": result.backend,
                "parameters": params,
                "upstream_wcs_source": frame.metadata.upstream_wcs.as_ref().map(|wcs| wcs.source),
                "panstarrs_pca": pca,
                "calibration": frame.metadata.calibration_provenance,
                "gain_e_per_adu": frame.metadata.gain_e_per_adu,
                "read_noise_e": frame.metadata.read_noise_e,
                "catalog": catalog.catalog,
                "catalog_query": catalog.query,
            }),
        )
    };
    let run_id = uuid::Uuid::new_v4().to_string();
    let frame_sha256 = crate::project::sha256_file(std::path::Path::new(&frame_path))?;
    let created_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut runs = state
        .reduction_runs
        .lock()
        .map_err(|error| error.to_string())?;
    for run in runs.values_mut().filter(|run| {
        run.get("frame_path").and_then(|value| value.as_str()) == Some(frame_path.as_str())
    }) {
        run["active"] = serde_json::Value::Bool(false);
    }
    runs.insert(run_id.clone(),serde_json::json!({"run_id":run_id,"frame_path":frame_path,"frame_sha256":frame_sha256,"created_unix":created_unix,"status":result.status.as_str(),"result":result,"catalog":catalog,"provenance":provenance,"active":true}));
    result.run_id = Some(run_id);
    Ok(())
}

#[tauri::command]
pub fn get_reduction_run(
    state: State<AppState>,
    run_id: String,
) -> Result<serde_json::Value, String> {
    state
        .reduction_runs
        .lock()
        .map_err(|error| error.to_string())?
        .get(&run_id)
        .cloned()
        .ok_or_else(|| "reduction run is no longer available in this session".to_string())
}

#[tauri::command]
pub async fn start_reduction(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: StartReductionRequest,
    on_progress: Channel<BatchReductionProgress>,
) -> Result<BatchReductionResult, String> {
    reduce_all_frames(app, state, request.params, on_progress).await
}

#[tauri::command]
pub fn refit_reduction(
    state: State<AppState>,
    run_id: String,
    excluded_match_ids: Vec<usize>,
) -> Result<platesolve::PlateSolveResult, String> {
    if !state
        .reduction_runs
        .lock()
        .map_err(|error| error.to_string())?
        .contains_key(&run_id)
    {
        return Err("reduction run is no longer available in this session".into());
    }
    let frame_index = *state
        .current_frame_index
        .lock()
        .map_err(|error| error.to_string())?;
    let (detection, catalog, seed) = {
        let analyses = state
            .frame_analyses
            .lock()
            .map_err(|error| error.to_string())?;
        let analysis = analyses
            .get(frame_index)
            .ok_or("Invalid frame analysis index")?;
        (
            analysis.detection.clone().ok_or("本帧尚未完成星点提取")?,
            analysis.catalog.clone().ok_or("本帧尚未查询 Gaia 星表")?,
            analysis
                .solution
                .as_ref()
                .and_then(|solution| solution.wcs.clone())
                .ok_or("本帧没有可供重新拟合的候选 WCS")?,
        )
    };
    let excluded: std::collections::HashSet<usize> = excluded_match_ids.into_iter().collect();
    let kept_indices: Vec<usize> = (0..detection.astrometry_stars.len())
        .filter(|index| !excluded.contains(index))
        .collect();
    let kept_sources: Vec<_> = kept_indices
        .iter()
        .map(|index| detection.astrometry_stars[*index].clone())
        .collect();
    let mut astrometric = matcher::refine_from_wcs_seed(
        &kept_sources,
        &catalog.sources,
        seed,
        matcher::MatchConfig {
            max_image_sources: kept_sources.len(),
            max_catalog_sources: 512,
            ..matcher::MatchConfig::default()
        },
    )
    .map_err(|error| error.to_string())?;
    for matched in &mut astrometric.matches {
        matched.image_source_index = kept_indices[matched.image_source_index];
    }
    let mut result = platesolve::solved(
        catalog.sources.len() as u32,
        astrometric,
        &detection.astrometry_stars,
    );
    persist_reduction_result(
        &state,
        frame_index,
        &SolveParams::default(),
        &catalog,
        &mut result,
    )?;
    let mut analyses = state
        .frame_analyses
        .lock()
        .map_err(|error| error.to_string())?;
    analyses[frame_index].solution = Some(result.clone());
    analyses[frame_index].photometry = None;
    analyses[frame_index].photometry_error = None;
    Ok(result)
}

#[tauri::command]
pub fn export_solved_fits(
    state: State<AppState>,
    run_id: String,
    destination: String,
) -> Result<(), String> {
    let run = state
        .reduction_runs
        .lock()
        .map_err(|error| error.to_string())?
        .get(&run_id)
        .cloned()
        .ok_or("reduction run is no longer available in this session")?;
    if run.get("status").and_then(|value| value.as_str()) != Some("accepted") {
        return Err("only accepted reductions may be exported".to_string());
    }
    let source = run
        .get("frame_path")
        .and_then(|value| value.as_str())
        .ok_or("stored reduction has no source frame")?;
    let source_path = std::path::Path::new(source);
    let destination_path = std::path::Path::new(&destination);
    if source_path == destination_path {
        return Err("the original FITS is immutable; choose a different export path".to_string());
    }

    #[cfg(not(feature = "cfitsio-backend"))]
    {
        let _ = destination_path;
        Err(
            "this build does not include the CFITSIO export backend; rebuild with --features cfitsio-backend"
                .to_string(),
        )
    }

    #[cfg(feature = "cfitsio-backend")]
    {
        let wcs = run
            .pointer("/result/wcs")
            .ok_or("accepted reduction has no WCS")?;
        let number = |name: &str| {
            wcs.get(name)
                .and_then(|value| value.as_f64())
                .ok_or_else(|| format!("stored WCS is missing {name}"))
        };
        let sip = wcs
            .get("sip")
            .map(|value| {
                serde_json::from_value::<crate::astrometry::wcs::SipDistortion>(value.clone())
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        std::fs::copy(source_path, destination_path).map_err(|error| error.to_string())?;
        let write_result = (|| -> Result<(), String> {
            let mut file =
                fitsio::FitsFile::edit(destination_path).map_err(|error| error.to_string())?;
            let hdu = file.primary_hdu().map_err(|error| error.to_string())?;
            for (key, value) in [
                ("CRPIX1", number("crpix1")? + 1.0),
                ("CRPIX2", number("crpix2")? + 1.0),
                ("CRVAL1", number("crval1")?),
                ("CRVAL2", number("crval2")?),
                ("CD1_1", number("cd1_1")?),
                ("CD1_2", number("cd1_2")?),
                ("CD2_1", number("cd2_1")?),
                ("CD2_2", number("cd2_2")?),
            ] {
                hdu.write_key(&mut file, key, value)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(sip) = &sip {
                hdu.write_key(&mut file, "CTYPE1", "RA---TAN-SIP")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "CTYPE2", "DEC--TAN-SIP")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "A_ORDER", sip.a_order as i32)
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "B_ORDER", sip.b_order as i32)
                    .map_err(|error| error.to_string())?;
                for (p, q, value) in sip.a_terms() {
                    hdu.write_key(&mut file, &format!("A_{p}_{q}"), value)
                        .map_err(|error| error.to_string())?;
                }
                for (p, q, value) in sip.b_terms() {
                    hdu.write_key(&mut file, &format!("B_{p}_{q}"), value)
                        .map_err(|error| error.to_string())?;
                }
                hdu.write_key(&mut file, "CUNIT1", "deg")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "CUNIT2", "deg")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "WCSNAME", "SKYEYE TAN/CD/SIP")
                    .map_err(|error| error.to_string())?;
            } else {
                hdu.write_key(&mut file, "CTYPE1", "RA---TAN")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "CTYPE2", "DEC--TAN")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "CUNIT1", "deg")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "CUNIT2", "deg")
                    .map_err(|error| error.to_string())?;
                hdu.write_key(&mut file, "WCSNAME", "SKYEYE TAN/CD")
                    .map_err(|error| error.to_string())?;
            }
            hdu.write_key(
                &mut file,
                "HISTORY",
                format!(
                    "SkyEye {} accepted reduction {run_id}",
                    env!("CARGO_PKG_VERSION")
                ),
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(destination_path);
            return Err(error);
        }
        Ok(())
    }
}

fn wcs_footprint_radius(wcs: &crate::astrometry::wcs::Wcs) -> f64 {
    let samples = [
        (0.0, 0.0),
        (f64::from(wcs.image_width), 0.0),
        (0.0, f64::from(wcs.image_height)),
        (f64::from(wcs.image_width), f64::from(wcs.image_height)),
        (f64::from(wcs.image_width) * 0.5, 0.0),
        (
            f64::from(wcs.image_width) * 0.5,
            f64::from(wcs.image_height),
        ),
        (0.0, f64::from(wcs.image_height) * 0.5),
        (
            f64::from(wcs.image_width),
            f64::from(wcs.image_height) * 0.5,
        ),
    ];
    samples
        .into_iter()
        .map(|(x, y)| {
            let (ra, dec) = wcs.pixel_to_sky(x, y);
            angular_separation_deg(wcs.crval1, wcs.crval2, ra, dec)
        })
        .fold(0.01, f64::max)
        .mul_add(1.15, 0.0)
        .clamp(0.01, 2.0)
}

fn angular_separation_deg(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let (ra1, dec1, ra2, dec2) = (
        ra1.to_radians(),
        dec1.to_radians(),
        ra2.to_radians(),
        dec2.to_radians(),
    );
    (dec1.sin() * dec2.sin() + dec1.cos() * dec2.cos() * (ra1 - ra2).cos())
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

#[tauri::command]
pub async fn reduce_all_frames(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    params: SolveParams,
    on_progress: Channel<BatchReductionProgress>,
) -> Result<BatchReductionResult, String> {
    let total = state
        .loaded_frames
        .lock()
        .map_err(|error| error.to_string())?
        .len();
    if total == 0 {
        return Err("No frames loaded".to_string());
    }
    let session_id = uuid::Uuid::new_v4().to_string();
    // Automatic reduction is committed only when the entire WCS + photometry
    // pass succeeds. Keep both mutable stores so a photometric failure cannot
    // leave an apparently solved session behind. Astrometric failures are kept
    // as staging data because the manual-calibration workflow consumes them.
    let analyses_before = state
        .frame_analyses
        .lock()
        .map_err(|error| error.to_string())?
        .clone();
    let reduction_runs_before = state
        .reduction_runs
        .lock()
        .map_err(|error| error.to_string())?
        .clone();
    let settings = crate::storage::load_config(&app)?;
    let mut params = params;
    if params.pixel_scale_arcsec.is_none() {
        params.pixel_scale_arcsec = settings
            .instrument
            .focal_length_mm
            .zip(settings.instrument.pixel_width_um)
            .and_then(|(focal, pixel)| {
                (focal > 0.0 && pixel > 0.0).then_some(206.265 * pixel / focal)
            });
    }
    if params.rotation_deg.is_none() {
        params.rotation_deg = settings.instrument.position_angle_deg;
    }
    if params.parity_flipped.is_none()
        && settings.instrument.flip_horizontal != settings.instrument.flip_vertical
    {
        params.parity_flipped = Some(true);
    }
    if params.catalog_bright_limit_mag.is_none() {
        params.catalog_bright_limit_mag = Some(settings.reduction.catalog_bright_limit_mag as f32);
    }
    if params.catalog_faint_limit_mag.is_none() {
        params.catalog_faint_limit_mag = Some(settings.reduction.catalog_faint_limit_mag as f32);
    }
    if params.maximum_reference_stars.is_none() {
        params.maximum_reference_stars = Some(settings.reduction.maximum_reference_stars as usize);
    }
    if params.astrometric_residual_limit_arcsec.is_none() {
        params.astrometric_residual_limit_arcsec =
            Some(settings.reduction.astrometric_residual_limit_arcsec);
    }
    if params.plate_model.is_none() {
        params.plate_model = Some(match settings.reduction.plate_model.as_str() {
            "quadratic" => matcher::PlateModel::Quadratic,
            "cubic" => matcher::PlateModel::Cubic,
            _ => matcher::PlateModel::Linear,
        });
    }

    let logs_dir = crate::storage::layout(&app)?.logs_dir;
    let mut session_log = match crate::logging::ReductionSessionLog::create(&logs_dir, &session_id)
    {
        Ok(mut session) => {
            session.raw("Sky Eye Reduction Session");
            session.raw(format!("Run ID: {session_id}"));
            session.raw(format!("Sky Eye version: {}", env!("CARGO_PKG_VERSION")));
            session.raw(format!("Frame count: {total}"));
            if let Ok(effective_settings) = serde_json::to_string_pretty(&serde_json::json!({
                "instrument": &settings.instrument,
                "reduction": &settings.reduction,
                "photometry": &settings.photometry,
                "solve_params": &params,
            })) {
                session.raw("Effective settings:");
                session.raw(effective_settings);
            }
            if let Ok(frames) = state.loaded_frames.lock() {
                for (index, frame) in frames.summaries().enumerate() {
                    session.line(
                        log::Level::Info,
                        &format!("frame {}/{} input", index + 1, total),
                        format!(
                            "path={} dimensions={}x{} date_obs={:?} exposure_s={:?} filter={:?} object={:?} center_ra_deg={:?} center_dec_deg={:?}",
                            frame.path,
                            frame.width,
                            frame.height,
                            frame.metadata.date_obs,
                            frame.metadata.exposure,
                            frame.metadata.filter,
                            frame.metadata.object,
                            frame.metadata.ra,
                            frame.metadata.dec,
                        ),
                    );
                }
            }
            log::info!(
                "reduction session started run={} frames={} session_log={}",
                session_id,
                total,
                session.path().display()
            );
            Some(session)
        }
        Err(error) => {
            log::error!("run={session_id} failed to create reduction session log: {error}");
            None
        }
    };
    let session_log_path = session_log
        .as_ref()
        .map(|session| session.path().to_string_lossy().into_owned());

    let mut results = Vec::with_capacity(total);
    let mut previous_wcs: Option<crate::astrometry::wcs::Wcs> = None;
    for frame_index in 0..total {
        if let Some(analysis) = state
            .frame_analyses
            .lock()
            .map_err(|error| error.to_string())?
            .get_mut(frame_index)
        {
            analysis.photometry = None;
            analysis.photometry_error = None;
        }
        publish_reduction_progress(
            &on_progress,
            &mut session_log,
            &session_id,
            BatchReductionProgress {
                frame_index,
                total,
                phase: "detection",
                message: format!("归算 {}/{}：提取星点", frame_index + 1, total),
            },
        );
        let detection = match detect_frame_async(
            &state,
            frame_index,
            settings.reduction.detection_sigma as f32,
            settings.reduction.minimum_fwhm_px,
            settings.reduction.maximum_reference_stars as usize,
            settings.instrument.saturation_adu,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                let solution = platesolve::match_failed(0, format!("星点提取失败：{error}"));
                store_failed_solution(&state, frame_index, solution.clone())?;
                publish_reduction_progress(
                    &on_progress,
                    &mut session_log,
                    &session_id,
                    BatchReductionProgress {
                        frame_index,
                        total,
                        phase: "failed",
                        message: solution.message.clone(),
                    },
                );
                results.push(FrameReductionResult {
                    frame_index,
                    detection: None,
                    solution,
                    photometry: None,
                    photometry_error: None,
                });
                continue;
            }
        };
        log::info!(
            "run={} frame={}/{} detection sources={} astrometry_sources={} background={:.3} noise={:.3} backend={}",
            session_id,
            frame_index + 1,
            total,
            detection.num_stars,
            detection.astrometry_stars.len(),
            detection.background,
            detection.noise,
            detection.backend
        );
        if let Some(session) = session_log.as_mut() {
            session.line(
                log::Level::Info,
                &format!("frame {}/{} detection-result", frame_index + 1, total),
                format!(
                    "sources={} astrometry_sources={} background={:.3} noise={:.3} backend={}",
                    detection.num_stars,
                    detection.astrometry_stars.len(),
                    detection.background,
                    detection.noise,
                    detection.backend
                ),
            );
        }

        let frame_hints = {
            let mut frames = state
                .loaded_frames
                .lock()
                .map_err(|error| error.to_string())?;
            let frame = frames.get(frame_index)?;
            (
                frame.metadata.ra,
                frame.metadata.dec,
                frame.metadata.pixel_scale_arcsec,
                frame.metadata.rotation_deg,
                frame.metadata.parity_flipped,
            )
        };
        let mut frame_params = params.clone();
        if let Some(wcs) = &previous_wcs {
            let (ra, dec) = wcs.pixel_to_sky(
                f64::from(wcs.image_width) * 0.5,
                f64::from(wcs.image_height) * 0.5,
            );
            let determinant = wcs.cd1_1 * wcs.cd2_2 - wcs.cd1_2 * wcs.cd2_1;
            // A previous accepted frame is only a fallback for missing metadata.
            // Never replace the current frame's own FITS pointing or geometry.
            if frame_params.center_ra_deg.is_none()
                && frame_params.center_dec_deg.is_none()
                && frame_hints.0.is_none()
                && frame_hints.1.is_none()
            {
                frame_params.center_ra_deg = Some(ra);
                frame_params.center_dec_deg = Some(dec);
                frame_params.radius_deg = None;
            }
            if frame_params.pixel_scale_arcsec.is_none() && frame_hints.2.is_none() {
                frame_params.pixel_scale_arcsec = Some(determinant.abs().sqrt() * 3_600.0);
            }
            if frame_params.rotation_deg.is_none() && frame_hints.3.is_none() {
                frame_params.rotation_deg = Some(wcs.cd2_1.atan2(wcs.cd1_1).to_degrees());
            }
            if frame_params.parity_flipped.is_none() && frame_hints.4.is_none() {
                frame_params.parity_flipped = Some(determinant < 0.0);
            }
        }

        publish_reduction_progress(
            &on_progress,
            &mut session_log,
            &session_id,
            BatchReductionProgress {
                frame_index,
                total,
                phase: "matching",
                message: format!("归算 {}/{}：匹配 Gaia 并拟合 WCS", frame_index + 1, total),
            },
        );
        let solution = match solve_frame(&state, frame_index, frame_params).await {
            Ok(solution) => solution,
            Err(error) => {
                let solution = platesolve::match_failed(0, error);
                store_failed_solution(&state, frame_index, solution.clone())?;
                solution
            }
        };
        if solution.success {
            previous_wcs = solution.wcs.clone();
        }
        let solve_level = if solution.success {
            log::Level::Info
        } else {
            log::Level::Warn
        };
        let wcs_summary = solution.wcs.as_ref().map(|wcs| {
            let determinant = wcs.cd1_1 * wcs.cd2_2 - wcs.cd1_2 * wcs.cd2_1;
            format!(
                "center=({:.8},{:.8}) scale={:.5}arcsec/px rotation={:.3}deg parity_flipped={}",
                wcs.crval1,
                wcs.crval2,
                determinant.abs().sqrt() * 3_600.0,
                wcs.cd2_1.atan2(wcs.cd1_1).to_degrees(),
                determinant < 0.0
            )
        });
        let quality_summary = solution.quality.as_ref().map(|quality| {
            format!(
                "matched={} rms={:.3}arcsec rms_ra={:.3}arcsec rms_dec={:.3}arcsec p95={:.3}arcsec",
                quality.matched,
                quality.residual_rms_arcsec,
                quality.rms_ra_arcsec,
                quality.rms_dec_arcsec,
                quality.residual_p95_arcsec
            )
        });
        let solve_summary = format!(
            "status={} catalog_sources={} {} {} message={}",
            solution.status.as_str(),
            solution.num_catalog,
            quality_summary.as_deref().unwrap_or("quality=unavailable"),
            wcs_summary.as_deref().unwrap_or("wcs=unavailable"),
            solution.message
        );
        log::log!(
            solve_level,
            "run={} frame={}/{} astrometry {}",
            session_id,
            frame_index + 1,
            total,
            solve_summary
        );
        if let Some(session) = session_log.as_mut() {
            session.line(
                solve_level,
                &format!("frame {}/{} astrometry-result", frame_index + 1, total),
                &solve_summary,
            );
        }
        let (photometry, photometry_error) = if solution.success {
            publish_reduction_progress(
                &on_progress,
                &mut session_log,
                &session_id,
                BatchReductionProgress {
                    frame_index,
                    total,
                    phase: "photometry",
                    message: format!(
                        "归算 {}/{}：使用 REFCAT2 进行光度定标",
                        frame_index + 1,
                        total
                    ),
                },
            );
            match solve_frame_photometry(&app, &state, frame_index, None).await {
                Ok(calibrated) => (Some(calibrated), None),
                Err(error) => {
                    if let Some(analysis) = state
                        .frame_analyses
                        .lock()
                        .map_err(|e| e.to_string())?
                        .get_mut(frame_index)
                    {
                        analysis.photometry = None;
                        analysis.photometry_error = Some(error.clone());
                    }
                    (None, Some(error))
                }
            }
        } else {
            (None, None)
        };
        if let Some(calibrated) = &photometry {
            let summary = format!(
                "status={} catalog=ATLAS2 band={} reference_stars={} zero_point={:.4} rms={:.4}mag color_term={:?} catalog_sha256={}",
                if calibrated.solution.accepted { "accepted" } else { "rejected" },
                calibrated.solution.band,
                calibrated.solution.reference_stars,
                calibrated.solution.zero_point,
                calibrated.solution.rms_mag,
                calibrated.solution.color_term,
                calibrated.catalog_sha256
            );
            log::info!(
                "run={} frame={}/{} photometry {}",
                session_id,
                frame_index + 1,
                total,
                summary
            );
            if let Some(session) = session_log.as_mut() {
                session.line(
                    log::Level::Info,
                    &format!("frame {}/{} photometry-result", frame_index + 1, total),
                    summary,
                );
            }
        } else if let Some(error) = &photometry_error {
            log::warn!(
                "run={} frame={}/{} photometry failed: {}",
                session_id,
                frame_index + 1,
                total,
                error
            );
            if let Some(session) = session_log.as_mut() {
                session.line(
                    log::Level::Warn,
                    &format!("frame {}/{} photometry-result", frame_index + 1, total),
                    format!("status=failed error={error}"),
                );
            }
        }
        let completion_message = match (&photometry, &photometry_error) {
            (Some(calibrated), _) => format!(
                "{}；光度定标完成：{} 颗参考星，ZP {:.3}",
                solution.message,
                calibrated.solution.reference_stars,
                calibrated.solution.zero_point
            ),
            (None, Some(error)) => format!("{}；光度定标不可用：{}", solution.message, error),
            _ => solution.message.clone(),
        };
        publish_reduction_progress(
            &on_progress,
            &mut session_log,
            &session_id,
            BatchReductionProgress {
                frame_index,
                total,
                phase: if solution.success { "solved" } else { "failed" },
                message: completion_message,
            },
        );
        results.push(FrameReductionResult {
            frame_index,
            detection: Some(detection),
            solution,
            photometry,
            photometry_error,
        });
    }

    let solved = results
        .iter()
        .filter(|result| result.solution.success)
        .count();
    let automatic_photometry_failed = solved == total
        && results
            .iter()
            .any(|result| result.photometry_error.is_some());
    if automatic_photometry_failed {
        *state
            .frame_analyses
            .lock()
            .map_err(|error| error.to_string())? = analyses_before;
        *state
            .reduction_runs
            .lock()
            .map_err(|error| error.to_string())? = reduction_runs_before;
    }
    let session_status = if automatic_photometry_failed {
        "rolled_back"
    } else if solved == total {
        "completed"
    } else {
        "review_required"
    };
    let summary = format!(
        "run_id={} frames={} solved={} failed={} automatic_photometry_rollback={}",
        session_id,
        total,
        solved,
        total - solved,
        automatic_photometry_failed
    );
    log::info!("reduction session ended status={session_status} {summary}");
    if let Some(session) = session_log.as_mut() {
        session.finish(session_status, &summary);
    }
    Ok(BatchReductionResult {
        failed: total - solved,
        solved,
        frames: results,
        session_id,
        session_log_path,
    })
}

fn store_failed_solution(
    state: &AppState,
    frame_index: usize,
    solution: platesolve::PlateSolveResult,
) -> Result<(), String> {
    let mut analyses = state.frame_analyses.lock().map_err(|e| e.to_string())?;
    let analysis = analyses
        .get_mut(frame_index)
        .ok_or("Invalid frame analysis index")?;
    analysis.solution = Some(solution);
    analysis.photometry = None;
    analysis.photometry_error = None;
    Ok(())
}

fn estimate_cone_radius(data: &fits::reader::FitsData) -> Option<f64> {
    if let Some(scale_arcsec) = data.metadata.pixel_scale_arcsec {
        let diagonal_pixels = f64::from(data.width).hypot(f64::from(data.height));
        return Some((0.5 * diagonal_pixels * scale_arcsec / 3_600.0 * 1.15).clamp(0.01, 2.0));
    }
    let focal_length_mm = data.metadata.focal_len?;
    let pixel_size_um = data.metadata.pixel_size?;
    if focal_length_mm <= 0.0 || pixel_size_um <= 0.0 {
        return None;
    }
    let radians_per_pixel = pixel_size_um / 1_000.0 / focal_length_mm;
    let diagonal_pixels = f64::from(data.width).hypot(f64::from(data.height));
    Some(
        (0.5 * diagonal_pixels * radians_per_pixel)
            .to_degrees()
            .mul_add(1.15, 0.0)
            .clamp(0.01, 2.0),
    )
}

#[tauri::command]
pub fn blink_set_frame(state: State<AppState>, index: usize) -> Result<BlinkState, String> {
    let frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
    let mut idx = state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    if index >= frames.len() {
        return Err("Frame index out of range".to_string());
    }
    *idx = index;
    let playing = *state.blink_playing.lock().map_err(|e| e.to_string())?;
    let speed = *state.blink_speed_ms.lock().map_err(|e| e.to_string())?;
    Ok(BlinkState {
        current_index: *idx,
        playing,
        speed_ms: speed,
    })
}

#[tauri::command]
pub fn blink_next(state: State<AppState>) -> Result<BlinkState, String> {
    let frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
    let mut idx = state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    if !frames.is_empty() {
        *idx = (*idx + 1) % frames.len();
    }
    let playing = *state.blink_playing.lock().map_err(|e| e.to_string())?;
    let speed = *state.blink_speed_ms.lock().map_err(|e| e.to_string())?;
    Ok(BlinkState {
        current_index: *idx,
        playing,
        speed_ms: speed,
    })
}

#[tauri::command]
pub fn blink_prev(state: State<AppState>) -> Result<BlinkState, String> {
    let frames = state.loaded_frames.lock().map_err(|e| e.to_string())?;
    let mut idx = state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    if !frames.is_empty() {
        *idx = if *idx == 0 {
            frames.len() - 1
        } else {
            *idx - 1
        };
    }
    let playing = *state.blink_playing.lock().map_err(|e| e.to_string())?;
    let speed = *state.blink_speed_ms.lock().map_err(|e| e.to_string())?;
    Ok(BlinkState {
        current_index: *idx,
        playing,
        speed_ms: speed,
    })
}

#[tauri::command]
pub fn blink_toggle(state: State<AppState>) -> Result<bool, String> {
    let mut playing = state.blink_playing.lock().map_err(|e| e.to_string())?;
    *playing = !*playing;
    Ok(*playing)
}

#[tauri::command]
pub fn blink_set_speed(state: State<AppState>, speed_ms: u64) -> Result<(), String> {
    let mut speed = state.blink_speed_ms.lock().map_err(|e| e.to_string())?;
    *speed = speed_ms;
    Ok(())
}

#[tauri::command]
pub fn blink_get_state(state: State<AppState>) -> Result<BlinkState, String> {
    let idx = *state
        .current_frame_index
        .lock()
        .map_err(|e| e.to_string())?;
    let playing = *state.blink_playing.lock().map_err(|e| e.to_string())?;
    let speed = *state.blink_speed_ms.lock().map_err(|e| e.to_string())?;
    Ok(BlinkState {
        current_index: idx,
        playing,
        speed_ms: speed,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        automatic_match_config, detect_frame_async, detection_revision,
        midpoint_adjustment_seconds, midpoint_rfc3339, reference_count_attempts,
        seeded_match_config, solve_frame, AppState, DetectionRevisionInput, FrameAnalysis,
        SolveParams, DETECTION_ALGORITHM_VERSION,
    };
    use crate::astrometry::matcher::PlateModel;
    use crate::{fits, project::sha256_file};
    use serde::Deserialize;
    use std::path::PathBuf;

    fn revision(
        threshold_sigma: f32,
        minimum_fwhm_px: f64,
        maximum_reference_stars: usize,
        saturation_level: Option<f64>,
    ) -> String {
        let frame = fits::registry::FrameIdentity {
            canonical_path: "C:/fixtures/frame.fits".into(),
            file_len: 4096,
            modified_ns: 123,
            sha256: "abc".into(),
            selected_hdu: 2,
            reader_version: fits::registry::FITS_READER_VERSION,
        };
        detection_revision(&DetectionRevisionInput {
            frame: &frame,
            threshold_sigma,
            minimum_fwhm_px,
            maximum_reference_stars,
            saturation_level,
            saturation_source: if saturation_level.is_some() {
                "configured"
            } else {
                "none"
            },
            algorithm_version: DETECTION_ALGORITHM_VERSION,
        })
        .expect("revision hash")
    }

    #[test]
    fn detection_revision_tracks_every_scientific_parameter() {
        let baseline = revision(4.0, 0.7, 120, Some(60_000.0));
        assert_eq!(baseline, revision(4.0, 0.7, 120, Some(60_000.0)));
        assert_ne!(baseline, revision(5.0, 0.7, 120, Some(60_000.0)));
        assert_ne!(baseline, revision(4.0, 1.2, 120, Some(60_000.0)));
        assert_ne!(baseline, revision(4.0, 0.7, 80, Some(60_000.0)));
        assert_ne!(baseline, revision(4.0, 0.7, 120, Some(50_000.0)));
    }

    #[derive(Deserialize)]
    struct GoldenManifest {
        frames: Vec<GoldenFrame>,
    }

    #[test]
    fn reference_count_retries_expand_without_exceeding_available_sources() {
        assert_eq!(
            reference_count_attempts(3, 200, PlateModel::Linear),
            Vec::<usize>::new()
        );
        assert_eq!(
            reference_count_attempts(30, 200, PlateModel::Linear),
            vec![30]
        );
        assert_eq!(
            reference_count_attempts(70, 200, PlateModel::Linear),
            vec![50, 70]
        );
        assert_eq!(
            reference_count_attempts(160, 200, PlateModel::Linear),
            vec![50, 90, 140, 160]
        );
        assert_eq!(
            reference_count_attempts(500, 200, PlateModel::Linear),
            vec![50, 90, 140, 200]
        );
        assert_eq!(
            reference_count_attempts(500, 100, PlateModel::Linear),
            vec![50, 90, 100]
        );
        assert_eq!(
            reference_count_attempts(500, 500, PlateModel::Cubic),
            vec![50, 90, 140, 200, 280, 360, 500]
        );
    }

    #[test]
    fn plate_model_reaches_match_config_from_solve_params() {
        let params = SolveParams {
            plate_model: Some(PlateModel::Cubic),
            ..SolveParams::default()
        };
        let seeded = seeded_match_config(&params, 120, 0.5, 400);
        let automatic = automatic_match_config(&params, 120, 0.5, None, None, None);
        assert_eq!(seeded.plate_model, PlateModel::Cubic);
        assert_eq!(automatic.plate_model, PlateModel::Cubic);
        assert_eq!(automatic.max_image_sources, 120);
        assert_eq!(automatic.max_catalog_sources, 120);
    }

    #[test]
    fn report_midpoint_is_rounded_to_configured_time_precision() {
        let settings = crate::storage::TimeConfig::default();
        assert_eq!(
            midpoint_rfc3339("2025-09-16T10:01:53.966548Z", 45.0, &settings, None).as_deref(),
            Some("2025-09-16T10:02:16Z")
        );
    }

    #[test]
    fn standard_header_time_reference_overrides_date_obs_setting() {
        let settings = crate::storage::TimeConfig::default();
        assert_eq!(
            midpoint_adjustment_seconds(40.0, &settings, Some("average")),
            0.0
        );
        assert_eq!(
            midpoint_adjustment_seconds(40.0, &settings, Some("begin")),
            20.0
        );
    }

    #[derive(Deserialize)]
    struct GoldenFrame {
        file: String,
        sha256: String,
    }

    #[tokio::test]
    async fn mandatory_panstarrs_crops_load_and_detect_offline() {
        let manifest: GoldenManifest =
            serde_json::from_str(include_str!("../tests/golden/panstarrs-xy54-crop.json"))
                .expect("crop manifest");
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
        let state = AppState::new();
        {
            let mut loaded = state.loaded_frames.lock().unwrap();
            for frame in &manifest.frames {
                let path = root.join(&frame.file);
                assert_eq!(sha256_file(&path).expect("crop checksum"), frame.sha256);
                let data = load_golden(&path);
                assert_eq!((data.width, data.height), (1024, 1024));
                assert!(data.metadata.date_obs.is_some());
                loaded.push_loaded(data).expect("index crop");
            }
        }
        *state.frame_analyses.lock().unwrap() =
            vec![FrameAnalysis::default(); manifest.frames.len()];
        for (index, frame) in manifest.frames.iter().enumerate() {
            let detection = detect_frame_async(&state, index, 4.0, 0.7, 120, Some(60_000.0))
                .await
                .expect("offline golden detection");
            assert!(
                detection.astrometry_stars.len() >= 10,
                "{} only produced {} usable stars",
                frame.file,
                detection.astrometry_stars.len()
            );
        }
    }

    #[tokio::test]
    async fn reduces_panstarrs_golden_frames_when_configured() {
        let Ok(root) = std::env::var("SKYEYE_GOLDEN_DIR") else {
            return;
        };
        if std::env::var_os("SKYEYE_GOLDEN_ONLINE").is_none() {
            return;
        }
        let manifest: GoldenManifest =
            serde_json::from_str(include_str!("../tests/golden/panstarrs-chip.json"))
                .expect("golden manifest");
        let paths: Vec<PathBuf> = manifest
            .frames
            .iter()
            .map(|frame| PathBuf::from(&root).join(&frame.file))
            .collect();
        let before: Vec<String> = paths
            .iter()
            .map(|path| sha256_file(path).expect("source checksum"))
            .collect();

        let state = AppState::new();
        {
            let mut loaded = state.loaded_frames.lock().unwrap();
            for path in &paths {
                loaded
                    .push_loaded(load_golden(path))
                    .expect("index golden frame");
            }
        }
        *state.frame_analyses.lock().unwrap() = vec![FrameAnalysis::default(); paths.len()];
        for (index, frame) in manifest.frames.iter().enumerate() {
            let detection = detect_frame_async(&state, index, 4.0, 0.7, 120, Some(60_000.0))
                .await
                .expect("golden detection");
            assert!(
                detection.astrometry_stars.len() >= 20,
                "{} detections",
                frame.file
            );
            let solution = solve_frame(&state, index, SolveParams::default())
                .await
                .expect("golden solve");
            let quality = solution.quality.as_ref().expect("quality metrics");
            assert_eq!(
                solution.status,
                crate::astrometry::quality::ReductionStatus::Accepted,
                "{}: {}",
                frame.file,
                solution.message
            );
            assert!(quality.matched >= 20, "{} matched", frame.file);
            assert!(quality.residual_rms_arcsec <= 0.5, "{} RMS", frame.file);
            assert!(quality.residual_p95_arcsec <= 1.0, "{} P95", frame.file);
            assert!(quality.occupied_grid_cells >= 8, "{} coverage", frame.file);
        }

        for ((path, expected), original) in paths.iter().zip(&manifest.frames).zip(before) {
            assert_eq!(sha256_file(path).expect("final checksum"), original);
            assert_eq!(original, expected.sha256);
        }
    }

    fn load_golden(path: &std::path::Path) -> fits::reader::FitsData {
        fits::reader::load_fits(path.to_str().expect("UTF-8 path")).expect("read golden FITS")
    }
}
