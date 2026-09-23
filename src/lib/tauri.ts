import { Channel, invoke } from '@tauri-apps/api/core'
import type {
  DetectionResult,
  PlateSolveResult,
  BlinkState,
  FrameMeta,
  FrameAnalysis,
  GaiaQuery,
  GaiaQueryResult,
  BatchReductionProgress,
  BatchReductionResult,
} from '../types/phase2'

export interface LoadFramesResult {
  frames: FrameMeta[]
  total: number
}

export async function loadFrames(paths: string[]): Promise<LoadFramesResult> {
  return invoke('load_frames', { paths })
}

export async function closeAllImages(): Promise<void> {
  return invoke('close_all_images')
}

export type FrontendLogLevel = 'debug' | 'info' | 'warn' | 'error'
export function writeFrontendLog(level: FrontendLogLevel, message: string, context?: string): void {
  void invoke('write_frontend_log', { entry: { level, message, context } }).catch(() => {})
}

export async function getFramePixelBuffer(index: number): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>('get_frame_pixel_buffer', { index })
}

export async function detectStars(): Promise<DetectionResult> {
  return invoke('detect_stars')
}

export async function getFrameAnalysis(index: number): Promise<FrameAnalysis> {
  return invoke('get_frame_analysis', { index })
}

export async function queryGaia(requestId: string, query: GaiaQuery): Promise<GaiaQueryResult> {
  return invoke('query_gaia', { requestId, query })
}

export async function cancelGaiaQuery(requestId: string): Promise<boolean> {
  return invoke('cancel_gaia_query', { requestId })
}

export interface ApertureParams {
  x: number
  y: number
  radius?: number
  annulus_inner?: number
  annulus_outer?: number
}

export interface ApertureMeasurement {
  x: number
  y: number
  aperture_radius: number
  flux: number
  flux_error: number
  snr: number | null
  background_per_pixel: number
  aperture_area: number
  annulus_pixels: number
  flags: number
}

export async function measureAperture(params: ApertureParams): Promise<ApertureMeasurement> {
  return invoke('measure_aperture', { params })
}

export type MatchStatus = 'unmatched' | 'no_local_match' | 'probable' | 'ambiguous' | 'confirmed'
export interface TargetMeasurement {
  id: string
  frame_index: number
  frame_path: string
  wcs_run_id: string | null
  midpoint_utc: string | null
  midpoint_jd: number | null
  x: number
  y: number
  ra_deg: number | null
  dec_deg: number | null
  ra_uncertainty_arcsec: number | null
  dec_uncertainty_arcsec: number | null
  flux: number
  flux_error: number
  snr: number | null
  fwhm_px: number | null
  ellipticity: number | null
  aperture_radius_px: number
  flags: string[]
  magnitude: number | null
  magnitude_error: number | null
  band: string | null
  photometric_catalog: string | null
  designation: string
  match_status: MatchStatus
  stale: boolean
  stale_reason: string | null
  provenance: Record<string, unknown>
}
export interface MpcorbManifest {
  source_page: string
  download_url: string
  downloaded_unix: number
  sha256: string
  record_count: number
  parser_version: string
  etag: string | null
  last_modified: string | null
}
export interface EphemerisPoint {
  designation: string
  ra_deg: number
  dec_deg: number
  rate_ra_arcsec_min: number
  rate_dec_arcsec_min: number
  angular_speed_arcsec_min: number
  predicted_mag: number | null
  quality: 'online_precise' | 'local_prediction' | 'degraded_time'
  epoch_offset_days: number | null
}
export interface Observatory {
  longitude_deg_east: number
  latitude_deg: number
  altitude_m: number
  dut1_seconds?: number | null
}
export async function measureTarget(
  frameIndex: number,
  x: number,
  y: number,
): Promise<TargetMeasurement> {
  return invoke('measure_target', { request: { frame_index: frameIndex, x, y } })
}
export async function confirmTargetMeasurement(
  id: string,
  designation: string,
): Promise<TargetMeasurement> {
  return invoke('confirm_target_measurement', { id, designation })
}
export async function discardTargetMeasurement(id: string): Promise<void> {
  return invoke('discard_target_measurement', { id })
}
export interface PhotometricSolution {
  band: string
  zero_point: number
  color_term: number | null
  rms_mag: number
  reference_stars: number
  accepted: boolean
  reason: string
}
export async function calibrateFramePhotometry(
  frameIndex: number,
  band?: string,
): Promise<{
  solution: PhotometricSolution
  catalog_sha256: string
  matched_reference_stars: number
  measurements: TargetMeasurement[]
}> {
  return invoke('calibrate_frame_photometry', { request: { frame_index: frameIndex, band } })
}
export async function listTargetMeasurements(): Promise<TargetMeasurement[]> {
  return invoke('list_target_measurements')
}
export async function deleteTargetMeasurement(id: string): Promise<void> {
  return invoke('delete_target_measurement', { id })
}
export async function renameTargetMeasurement(
  id: string,
  name: string,
): Promise<TargetMeasurement> {
  return invoke('rename_target_measurement', { id, name })
}
export async function updateMpcorb(): Promise<MpcorbManifest> {
  return invoke('update_mpcorb')
}
export async function importMpcorb(sourcePath: string): Promise<MpcorbManifest> {
  return invoke('import_mpcorb', { sourcePath })
}
export async function getMpcorbStatus(): Promise<MpcorbManifest | null> {
  return invoke('get_mpcorb_status')
}
export async function searchKnownObjects(request: {
  jd_utc: number
  center_ra_deg: number
  center_dec_deg: number
  radius_deg: number
  station?: Observatory
  max_results?: number
}): Promise<{
  database: MpcorbManifest
  objects: EphemerisPoint[]
  source: 'jpl_second_pass' | 'mpcorb_local'
  warnings: string[]
}> {
  return invoke('search_known_objects', { request })
}
export async function searchKnownObjectsBatch(request: {
  frames: Array<{
    frame_index: number
    jd_utc: number
    center_ra_deg: number
    center_dec_deg: number
    radius_deg: number
  }>
  station?: Observatory
  max_results_per_frame?: number
}): Promise<{
  database: MpcorbManifest
  frames: Array<{
    frame_index: number
    objects: EphemerisPoint[]
    source: 'jpl_second_pass' | 'mpcorb_local'
    warnings: string[]
  }>
}> {
  return invoke('search_known_objects_batch', { request })
}
export async function matchTracklet(
  points: Array<{ measurement_id: string; jd_utc: number; ra_deg: number; dec_deg: number }>,
  station?: Observatory,
) {
  return invoke<{
    status: MatchStatus
    candidates: Array<{
      designation: string
      residuals_arcsec: number[]
      max_residual_arcsec: number
    }>
    reason: string
    database_stale: boolean
  }>('match_tracklet', { request: { points, station } })
}
export interface AdesContext {
  observatory_code: string
  submitter: string
  observers: string[]
  measurers: string[]
  telescope?: string
  telescope_aperture_m?: number
  detector?: string
  software_version: string
  position_precision_1e6_deg: boolean
  magnitude_precision_hundredth: boolean
  mpcorb_sha256?: string
  refcat2_sha256?: string
}
export interface AdesObservation {
  perm_id?: string
  prov_id?: string
  trk_sub?: string
  mode: string
  obs_time: string
  ra_deg: number
  dec_deg: number
  rms_ra_arcsec?: number
  rms_dec_arcsec?: number
  ast_cat: string
  mag?: number
  rms_mag?: number
  band?: string
  filter?: string
  phot_cat?: string
  phot_ap_arcsec?: number
  snr?: number
  seeing_arcsec?: number
  exposure_seconds?: number
  rms_fit_arcsec?: number
  astrometric_reference_stars?: number
  accepted_wcs: boolean
}
export async function previewAdes(
  context: AdesContext,
  observations: AdesObservation[],
): Promise<string> {
  return invoke('preview_ades', { request: { context, observations } })
}
export async function exportAdes(
  destination: string,
  context: AdesContext,
  observations: AdesObservation[],
): Promise<string> {
  return invoke('export_ades', { request: { destination, report: { context, observations } } })
}
export type ReportFormat = 'ades2022_psv' | 'mpc1992_80_column'
export interface ReportContext {
  observatory_code: string
  submitter: string
  observers: string[]
  measurers: string[]
  telescope?: string
  telescope_aperture_m?: number
  detector?: string
  software_version: string
  position_precision_1e6_deg: boolean
  magnitude_precision_hundredth: boolean
  mpcorb_sha256?: string
  refcat2_sha256?: string
}
export type ObjectIdentity = { kind: 'permanent' | 'provisional' | 'tracklet'; value: string }
export interface ReportObservation {
  identity: ObjectIdentity
  mode: string
  obs_time_utc: string
  ra_deg: number
  dec_deg: number
  ra_uncertainty_arcsec?: number
  dec_uncertainty_arcsec?: number
  astrometric_catalog: string
  magnitude?: number
  magnitude_uncertainty?: number
  band?: string
  filter?: string
  photometric_catalog?: string
  aperture_arcsec?: number
  snr?: number
  seeing_arcsec?: number
  exposure_seconds?: number
  rms_fit_arcsec?: number
  astrometric_reference_stars?: number
  accepted_wcs: boolean
}
export interface RenderedReport {
  content: string
  warnings: string[]
}
/**
 * Advisory notes raised while rendering, e.g. a magnitude band the MPC no
 * longer accepts or a name that does not look like "initials + surname".
 * They never block an export.
 */
export async function previewReport(
  format: ReportFormat,
  context: ReportContext,
  observations: ReportObservation[],
): Promise<RenderedReport> {
  return invoke('preview_report', { request: { format, context, observations } })
}
export async function exportReport(
  destination: string,
  format: ReportFormat,
  context: ReportContext,
  observations: ReportObservation[],
): Promise<string> {
  return invoke('export_report', {
    payload: { destination, request: { format, context, observations } },
  })
}

export interface AppConfig {
  schema_version: 1
  station: {
    mpc_code: string
    name: string
    longitude_deg_east?: number
    latitude_deg?: number
    altitude_m?: number
    dut1_seconds?: number
    eop_updated_unix?: number
    telescope?: string
    aperture_m?: number
    focal_ratio?: number
    detector: string
    observer_names: string[]
    measurer_names: string[]
  }
  submitter: string
  instrument: {
    focal_length_mm?: number
    focal_length_tolerance_percent: number
    pixel_width_um?: number
    pixel_height_um?: number
    position_angle_deg?: number
    position_angle_tolerance_deg: number
    pointing_tolerance_arcmin: number
    flip_horizontal: boolean
    flip_vertical: boolean
    auto_rotate_pierside: boolean
    saturation_adu?: number
  }
  time: {
    date_obs_reference: 'start' | 'midpoint' | 'end'
    exposure_unit: 'seconds' | 'milliseconds' | 'minutes'
    utc_offset_hours: number
    shutter_delay_seconds: number
    precision_seconds: number
    check_after_loading: boolean
  }
  reduction: {
    astrometry_catalog: 'Gaia3'
    detection_sigma: number
    minimum_fwhm_px: number
    maximum_centroid_fit_rms: number
    centroid_search_radius_px: number
    centroid_method: 'gaussian_window'
    plate_model: 'linear' | 'quadratic' | 'cubic'
    catalog_bright_limit_mag: number
    catalog_faint_limit_mag: number
    maximum_reference_stars: number
    initial_match_radius_px: number
    astrometric_residual_limit_arcsec: number
    alignment_reference_stars: number
  }
  photometry: {
    catalog: 'ATLAS2'
    reference_band: 'G' | 'g' | 'r' | 'i' | 'z'
    aperture_mode: 'adaptive' | 'fixed'
    aperture_fwhm_multiplier: number
    fixed_aperture_radius_px: number
    aperture_gap_px: number
    sky_annulus_inner_fwhm: number
    sky_annulus_outer_fwhm: number
    minimum_reference_stars: number
    maximum_catalog_error_mag: number
    maximum_residual_mag: number
    fit_color_term: boolean
  }
  report: {
    default_format: ReportFormat
    band: string
    include_magnitude: boolean
    position_precision_1e6_deg: boolean
    magnitude_precision_hundredth: boolean
    allow_artificial_satellites: boolean
  }
  data: {
    mpcorb_auto_update: boolean
    mpcorb_max_age_hours: number
    known_object_mba_limit_mag: number
    known_object_tno_limit_mag: number
    known_object_magnitude_offset: number
    jpl_mode: 'auto' | 'offline'
    jpl_timeout_seconds: number
  }
}
export async function getAppConfig(): Promise<AppConfig> {
  return invoke('get_app_config')
}
export async function saveAppConfig(
  config: AppConfig,
): Promise<{ scientific_cache_invalidated: boolean }> {
  return invoke('save_app_config', { config })
}

export async function loadAppConfigFile(path: string): Promise<AppConfig> {
  return invoke('load_app_config_file', { path })
}

export async function saveAppConfigFile(path: string, config: AppConfig): Promise<void> {
  return invoke('save_app_config_file', { path, config })
}

export interface StorageLayout {
  root: string
  config_dir: string
  data_dir: string
  mpcorb_dir: string
  cache_dir: string
  exports_dir: string
  logs_dir: string
  presets_dir: string
  settings_file: string
}

export async function getStorageLayout(): Promise<StorageLayout> {
  return invoke('get_storage_layout')
}

export interface SolveParams {
  center_ra_deg?: number
  center_dec_deg?: number
  radius_deg?: number
  pixel_scale_arcsec?: number
  rotation_deg?: number
  parity_flipped?: boolean
  offset_x_px?: number
  offset_y_px?: number
  catalog_bright_limit_mag?: number
  catalog_faint_limit_mag?: number
  maximum_reference_stars?: number
  astrometric_residual_limit_arcsec?: number
  accept_review?: boolean
}

export interface ManualCalibrationSeed {
  center_ra_deg: number
  center_dec_deg: number
  pixel_scale_arcsec: number
  rotation_deg: number
  parity_flipped: boolean
  offset_x_px: number
  offset_y_px: number
  catalog_bright_limit_mag: number
  catalog_faint_limit_mag: number
}

export async function plateSolve(params: SolveParams): Promise<PlateSolveResult> {
  return invoke('plate_solve', { params })
}

export async function reduceAllFrames(
  params: SolveParams,
  onProgress: (progress: BatchReductionProgress) => void,
): Promise<BatchReductionResult> {
  const channel = new Channel<BatchReductionProgress>()
  channel.onmessage = onProgress
  return invoke('reduce_all_frames', { params, onProgress: channel })
}

export async function startReduction(
  params: SolveParams,
  onProgress: (progress: BatchReductionProgress) => void,
): Promise<BatchReductionResult> {
  const channel = new Channel<BatchReductionProgress>()
  channel.onmessage = onProgress
  return invoke('start_reduction', { request: { params }, onProgress: channel })
}

export async function refitReduction(
  runId: string,
  excludedMatchIds: number[],
): Promise<PlateSolveResult> {
  return invoke('refit_reduction', { runId, excludedMatchIds })
}

export async function getReductionRun(runId: string): Promise<Record<string, unknown>> {
  return invoke('get_reduction_run', { runId })
}

export async function exportSolvedFits(runId: string, destination: string): Promise<void> {
  return invoke('export_solved_fits', { runId, destination })
}

export async function blinkSetFrame(index: number): Promise<BlinkState> {
  return invoke('blink_set_frame', { index })
}

export async function blinkNext(): Promise<BlinkState> {
  return invoke('blink_next')
}

export async function blinkPrev(): Promise<BlinkState> {
  return invoke('blink_prev')
}

export async function blinkSetSpeed(speedMs: number): Promise<void> {
  return invoke('blink_set_speed', { speedMs })
}

export async function blinkToggle(): Promise<boolean> {
  return invoke('blink_toggle')
}

export async function blinkGetState(): Promise<BlinkState> {
  return invoke('blink_get_state')
}
