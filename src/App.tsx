import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { open, save as saveDialog } from '@tauri-apps/plugin-dialog'
import { useShallow } from 'zustand/react/shallow'
import { FITSViewer, type FITSViewerHandle } from './components/FITSViewer'
import { TitleBar } from './components/TitleBar'
import { DisplaySidebar } from './components/DisplaySidebar'
import { SciencePanel } from './components/SciencePanel'
import { SettingsDialog } from './components/SettingsDialog'
import { ReportExportDialog } from './components/ReportExportDialog'
import { SuspiciousTargetDialog } from './components/SuspiciousTargetDialog'
import {
  countApproximateMatches,
  ManualCalibrationPanel,
  type ManualCalibrationState,
} from './components/ManualCalibration'
import { useFitsStore } from './stores/fitsStore'
import { useSessionStore } from './stores/sessionStore'
import {
  loadFrames,
  closeAllImages,
  getFramePixelBuffer,
  blinkGetState,
  blinkSetFrame,
  startReduction,
  getFrameAnalysis,
  plateSolve,
  calibrateFramePhotometry,
  getMpcorbStatus,
  matchTracklet,
  measureTarget,
  confirmTargetMeasurement,
  discardTargetMeasurement,
  searchKnownObjectsBatch,
  deleteTargetMeasurement,
  renameTargetMeasurement,
  updateMpcorb,
  importMpcorb,
  getAppConfig,
  saveAppConfig,
  previewReport,
  exportReport,
  writeFrontendLog,
} from './lib/tauri'
import type {
  AppConfig,
  EphemerisPoint,
  ManualCalibrationSeed,
  MpcorbManifest,
  ObjectIdentity,
  ReportContext,
  ReportFormat,
  ReportObservation,
  SolveParams,
  TargetMeasurement,
} from './lib/tauri'
import { Eye, EyeOff, FileOutput, FolderOpen, ImageOff, ListChecks, Orbit } from 'lucide-react'
import { Toolbar } from './components/ui/surface'
import { OperationDialog } from './components/ui/operation-dialog'
import { MessageDialog } from './components/ui/message-dialog'
import { IconButton, TooltipProvider } from './components/ui/tooltip'

function reportIdentity(designation: string): ObjectIdentity {
  return { kind: 'tracklet', value: designation }
}

const APP_STARTED_UNIX_SECONDS = Date.now() / 1000

function App() {
  const {
    error,
    isLoading,
    setMeta,
    setLoading,
    setError,
    setFilePath,
    requestFit,
    setStretchLimits,
  } = useFitsStore()
  const {
    detectedStars,
    isDetecting,
    isSolving,
    frames,
    framePixels,
    currentFrameIndex,
    frameAnalyses,
    isPlaying,
    setFrames,
    setBlinkState,
    cacheFramePixels,
    setDetection,
    setSolution,
    setFrameReduction,
    setIsDetecting,
    setIsSolving,
    resetSession,
  } = useSessionStore(
    // 精确订阅:无关字段(如 blinkPrep、speedMs)变化不再触发 App 全树更新
    useShallow((state) => ({
      detectedStars: state.detectedStars,
      isDetecting: state.isDetecting,
      isSolving: state.isSolving,
      frames: state.frames,
      framePixels: state.framePixels,
      currentFrameIndex: state.currentFrameIndex,
      frameAnalyses: state.frameAnalyses,
      isPlaying: state.isPlaying,
      setFrames: state.setFrames,
      setBlinkState: state.setBlinkState,
      cacheFramePixels: state.cacheFramePixels,
      setCurrentFrame: state.setCurrentFrame,
      setDetection: state.setDetection,
      setSolution: state.setSolution,
      setFrameReduction: state.setFrameReduction,
      setIsDetecting: state.setIsDetecting,
      setIsSolving: state.setIsSolving,
      resetSession: state.resetSession,
    })),
  )

  const pixelLoadGeneration = useRef(0)
  const pixelLoads = useRef(new Map<number, Promise<void>>())
  const viewerRef = useRef<FITSViewerHandle>(null)
  const blinkGenRef = useRef(0)
  const measureInFlight = useRef(false)
  const [loadingProgress, setLoadingProgress] = useState<string | null>(null)
  const [reductionMessage, setReductionMessage] = useState<string | null>(null)
  const [reductionProgress, setReductionProgress] = useState<string | null>(null)
  const [stretchMode, setStretchMode] = useState<'linear' | 'asinh'>('linear')
  const [inverted, setInverted] = useState(true)
  const [manualCalibration, setManualCalibration] = useState<ManualCalibrationState | null>(null)
  const [manualBusy, setManualBusy] = useState(false)
  const [manualQueue, setManualQueue] = useState<number[]>([])
  const [pendingMeasurement, setPendingMeasurement] = useState<TargetMeasurement | null>(null)
  const [scienceOpen, setScienceOpen] = useState(false)
  const [scienceBusy, setScienceBusy] = useState(false)
  const [measurements, setMeasurements] = useState<TargetMeasurement[]>([])
  const [mpcorb, setMpcorb] = useState<MpcorbManifest | null>(null)
  const [knownObjectsByFrame, setKnownObjectsByFrame] = useState<Record<number, EphemerisPoint[]>>(
    {},
  )
  const [knownVisible, setKnownVisible] = useState(false)
  const [knownSearchBusy, setKnownSearchBusy] = useState(false)
  const [appConfig, setAppConfig] = useState<AppConfig | null>(null)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [reportOpen, setReportOpen] = useState(false)
  const [reportFormat, setReportFormat] = useState<ReportFormat>('ades2022_psv')
  const [reportPreview, setReportPreview] = useState('')
  const [reportWarnings, setReportWarnings] = useState<string[]>([])
  const [reportError, setReportError] = useState<string | null>(null)
  const [reportBusy, setReportBusy] = useState(false)
  const manualFrameIndex = useRef<number | null>(null)
  const lastSolveParams = useRef<SolveParams>({})
  const currentWcsAccepted = frameAnalyses[currentFrameIndex]?.solution?.status === 'accepted'
  const canMeasure = currentWcsAccepted
  const knownPrerequisites = Boolean(
    frames.length > 0 &&
    mpcorb &&
    frames.every(
      (frame, index) =>
        frame.observation_midpoint_jd != null &&
        frameAnalyses[index]?.solution?.status === 'accepted',
    ),
  )
  const currentKnownObjects = knownObjectsByFrame[currentFrameIndex] ?? []
  const currentPixelFrame = framePixels[currentFrameIndex]
  const frameWidth = currentPixelFrame?.width ?? frames[currentFrameIndex]?.width ?? 0
  const frameHeight = currentPixelFrame?.height ?? frames[currentFrameIndex]?.height ?? 0
  const knownObjectCount = Object.values(knownObjectsByFrame).reduce(
    (total, objects) => total + objects.length,
    0,
  )
  // 必须 memo：reportObservations 以它为依赖，非 memo 会在每次渲染产生新数组，
  // 导致下面的预览 effect 依赖恒变 → 无限重跑（预览闪烁/错误弹窗死循环）。
  const reportMeasurements = useMemo(
    () => measurements.filter((measurement) => !measurement.stale),
    [measurements],
  )
  const reportContext = useMemo<ReportContext | null>(
    () =>
      appConfig
        ? {
            observatory_code: appConfig.station.mpc_code,
            submitter: appConfig.submitter,
            observers: appConfig.station.observer_names,
            measurers: appConfig.station.measurer_names,
            telescope: appConfig.station.telescope,
            telescope_aperture_m: appConfig.station.aperture_m,
            detector: appConfig.station.detector,
            software_version: 'SkyEye 0.1.0',
            position_precision_1e6_deg: appConfig.report.position_precision_1e6_deg,
            magnitude_precision_hundredth: appConfig.report.magnitude_precision_hundredth,
            mpcorb_sha256: mpcorb?.sha256,
            refcat2_sha256: measurements.find(
              (measurement) => typeof measurement.provenance.refcat2_sha256 === 'string',
            )?.provenance.refcat2_sha256 as string | undefined,
          }
        : null,
    [appConfig, measurements, mpcorb],
  )
  const reportObservations = useMemo<ReportObservation[]>(
    () =>
      reportMeasurements.map((measurement) => {
        const frame = frames[measurement.frame_index]
        const solution = frameAnalyses[measurement.frame_index]?.solution
        const quality = solution?.quality
        const wcs = solution?.wcs
        const pixelScale = wcs
          ? (Math.hypot(wcs.cd1_1, wcs.cd2_1) + Math.hypot(wcs.cd1_2, wcs.cd2_2)) * 1800
          : undefined
        const combineUncertainty = (centroid: number | null, fit: number | undefined) =>
          centroid == null && fit == null ? undefined : Math.hypot(centroid ?? 0, fit ?? 0)
        return {
          identity: reportIdentity(measurement.designation),
          mode: 'CCD',
          obs_time_utc: measurement.midpoint_utc ?? '',
          ra_deg: measurement.ra_deg ?? Number.NaN,
          dec_deg: measurement.dec_deg ?? Number.NaN,
          ra_uncertainty_arcsec: combineUncertainty(
            measurement.ra_uncertainty_arcsec,
            quality?.rms_ra_arcsec,
          ),
          dec_uncertainty_arcsec: combineUncertainty(
            measurement.dec_uncertainty_arcsec,
            quality?.rms_dec_arcsec,
          ),
          astrometric_catalog: 'Gaia3',
          magnitude: appConfig?.report.include_magnitude
            ? (measurement.magnitude ?? undefined)
            : undefined,
          magnitude_uncertainty: measurement.magnitude_error ?? undefined,
          band:
            appConfig?.report.include_magnitude && measurement.magnitude != null
              ? (measurement.band ?? undefined)
              : undefined,
          filter:
            appConfig?.report.include_magnitude &&
            measurement.magnitude != null &&
            appConfig.report.band !== 'C'
              ? appConfig.report.band
              : undefined,
          photometric_catalog: measurement.photometric_catalog ?? undefined,
          aperture_arcsec:
            pixelScale == null ? undefined : measurement.aperture_radius_px * pixelScale,
          snr: measurement.snr ?? undefined,
          seeing_arcsec:
            pixelScale == null || measurement.fwhm_px == null
              ? undefined
              : measurement.fwhm_px * pixelScale,
          exposure_seconds: frame?.exposure ?? undefined,
          rms_fit_arcsec: quality?.residual_rms_arcsec,
          astrometric_reference_stars: quality?.matched,
          accepted_wcs: solution?.status === 'accepted',
        }
      }),
    [appConfig, reportMeasurements, frameAnalyses, frames],
  )
  useEffect(() => {
    void getAppConfig()
      .then(setAppConfig)
      .catch((e) => setError(String(e)))
  }, [setError])
  useEffect(() => {
    if (!appConfig) return
    void getMpcorbStatus()
      .then((local) => {
        setMpcorb(local)
        if (
          appConfig.data.mpcorb_auto_update &&
          (!local ||
            Date.now() / 1000 - local.downloaded_unix > appConfig.data.mpcorb_max_age_hours * 3600)
        ) {
          void updateMpcorb()
            .then(setMpcorb)
            .catch(() => {
              /* offline startup retains the active snapshot */
            })
        }
      })
      .catch(() => {})
  }, [appConfig])
  useEffect(() => {
    if (!knownPrerequisites) setKnownVisible(false)
  }, [knownPrerequisites])

  const approximateManualMatches = useMemo(
    () =>
      manualCalibration
        ? countApproximateMatches(manualCalibration, detectedStars, frameWidth, frameHeight)
        : 0,
    [manualCalibration, detectedStars, frameWidth, frameHeight],
  )

  const ensureFramePixels = useCallback(
    async (index: number, generation = pixelLoadGeneration.current) => {
      const frame = useSessionStore.getState().frames[index]
      if (!frame) return
      if (useSessionStore.getState().framePixels[index]) return
      const existing = pixelLoads.current.get(index)
      if (existing) return existing
      const loading = (async () => {
        const buffer = await getFramePixelBuffer(index)
        if (generation !== pixelLoadGeneration.current) return
        const pixels = new Float32Array(buffer)
        cacheFramePixels(index, pixels, frame.width, frame.height)
      })().finally(() => pixelLoads.current.delete(index))
      pixelLoads.current.set(index, loading)
      return loading
    },
    [cacheFramePixels],
  )

  // 按 CPU 预算预热:当前帧、播放方向邻帧优先,长序列不再整组常驻。
  const prepareBlinkSet = useCallback(async () => {
    const gen = pixelLoadGeneration.current
    const state = useSessionStore.getState()
    const total = state.frames.length
    if (total < 2) {
      state.setBlinkPrep(null)
      return
    }
    const budget = state.pixelCacheStats.maxBytes
    const order = Array.from(
      { length: total },
      (_, offset) => (state.currentFrameIndex + offset) % total,
    )
    const planned: number[] = []
    let bytes = 0
    for (const index of order) {
      const frame = state.frames[index]
      const frameBytes = frame.width * frame.height * Float32Array.BYTES_PER_ELEMENT
      if (planned.length > 0 && bytes + frameBytes > budget) break
      planned.push(index)
      bytes += frameBytes
    }
    state.setBlinkPrep({ loaded: 0, total: planned.length })
    for (let position = 0; position < planned.length; position++) {
      const index = planned[position]
      await ensureFramePixels(index, gen)
      if (gen !== pixelLoadGeneration.current) return
      viewerRef.current?.prewarmFrame(index)
      if (gen !== pixelLoadGeneration.current) return
      useSessionStore.getState().setBlinkPrep({ loaded: position + 1, total: planned.length })
    }
    if (gen === pixelLoadGeneration.current) {
      useSessionStore.getState().setBlinkPrep(null)
    }
  }, [ensureFramePixels])

  useEffect(() => {
    if (!frames[currentFrameIndex]) return
    void ensureFramePixels(currentFrameIndex).catch((error) => setError(String(error)))
  }, [currentFrameIndex, ensureFramePixels, frames, setError])

  // 本地播放索引:rAF + 时间累加器,热路径零 IPC;暂停/停止时把前端当前帧同步回 Rust
  useEffect(() => {
    if (!isPlaying || frames.length < 2) return
    const gen = ++blinkGenRef.current
    let rafId = 0
    let lastSwitchAt = 0
    let switching = false
    const loop = (now: number) => {
      if (gen !== blinkGenRef.current) return
      if (!switching && now - lastSwitchAt >= useSessionStore.getState().speedMs) {
        const state = useSessionStore.getState()
        const next = (state.currentFrameIndex + 1) % state.frames.length
        switching = true
        void ensureFramePixels(next)
          .then(() => {
            if (gen !== blinkGenRef.current) return
            viewerRef.current?.prewarmFrame(next)
            useSessionStore.getState().setCurrentFrame(next)
            lastSwitchAt = performance.now()
            const following = (next + 1) % state.frames.length
            void ensureFramePixels(following).then(() => viewerRef.current?.prewarmFrame(following))
          })
          .catch((error) => setError(String(error)))
          .finally(() => {
            switching = false
          })
      }
      rafId = requestAnimationFrame(loop)
    }
    lastSwitchAt = performance.now()
    rafId = requestAnimationFrame(loop)
    return () => {
      cancelAnimationFrame(rafId)
      // 科学命令仍读 Rust current_frame_index:暂停/停止时同步一次
      void blinkSetFrame(useSessionStore.getState().currentFrameIndex).catch(() => {})
    }
  }, [ensureFramePixels, frames.length, isPlaying, setError])

  const handleOpen = useCallback(async () => {
    try {
      const selected = await open({
        multiple: true,
        filters: [{ name: 'FITS', extensions: ['fits', 'fit', 'fts', 'fz'] }],
      })
      if (!selected || selected.length === 0) return
      const paths = Array.isArray(selected) ? selected : [selected]
      setLoading(true)
      setError(null)
      setLoadingProgress(null)
      setReductionMessage(null)
      setManualCalibration(null)
      setManualQueue([])
      setMeasurements([])
      setKnownObjectsByFrame({})
      setKnownVisible(false)
      manualFrameIndex.current = null
      pixelLoadGeneration.current += 1
      pixelLoads.current.clear()
      setStretchLimits(null)

      const result = await loadFrames(paths)
      const first = result.frames[0]
      if (!first) {
        setError('No frames loaded')
        setLoading(false)
        return
      }

      setLoadingProgress('加载当前帧')
      setFrames(result.frames)
      for (const frame of result.frames) {
        for (const diagnostic of frame.diagnostics) {
          writeFrontendLog('warn', `${frame.label}: ${diagnostic}`, 'fits-header')
        }
      }
      setFilePath(first.path)
      setMeta({
        path: first.path,
        width: first.width,
        height: first.height,
        min_val: first.min_val,
        max_val: first.max_val,
        object: first.object,
        ra: first.ra,
        dec: first.dec,
        exposure: first.exposure,
        filter: first.filter,
        date_obs: first.date_obs,
        focal_length: first.focal_length,
        pixel_size: first.pixel_size,
        pixel_scale_arcsec: first.pixel_scale_arcsec,
        rotation_deg: first.rotation_deg,
        parity_flipped: first.parity_flipped,
      })

      const firstPixels = new Float32Array(await getFramePixelBuffer(0))
      cacheFramePixels(0, firstPixels, first.width, first.height)
      writeFrontendLog(
        'debug',
        `Frame 1/${result.total}: w=${first.width} h=${first.height} pixels=${firstPixels.length} min=${first.min_val} max=${first.max_val}`,
        'fits',
      )
      requestFit()

      // Set up Blink session if needed
      if (result.total >= 2) {
        const blinkState = await blinkGetState()
        setBlinkState(blinkState)
      }
      void prepareBlinkSet()
    } catch (err) {
      setError(String(err))
    } finally {
      setLoading(false)
      setLoadingProgress(null)
    }
  }, [
    setMeta,
    setLoading,
    setError,
    setFilePath,
    setFrames,
    setBlinkState,
    cacheFramePixels,
    requestFit,
    setStretchLimits,
    prepareBlinkSet,
  ])

  const handleCloseAllImages = useCallback(async () => {
    try {
      await closeAllImages()
      resetSession()
      setMeta(null)
      setFilePath(null)
      setLoading(false)
      setError(null)
      setMeasurements([])
      setKnownObjectsByFrame({})
      setKnownVisible(false)
      setPendingMeasurement(null)
      setScienceOpen(false)
      setReportOpen(false)
      setReportPreview('')
      setManualCalibration(null)
      setManualQueue([])
      manualFrameIndex.current = null
      pixelLoadGeneration.current += 1
      pixelLoads.current.clear()
      setStretchLimits(null)
      setLoadingProgress(null)
      setReductionProgress(null)
      setReductionMessage(null)
    } catch (e) {
      setError(String(e))
    }
  }, [resetSession, setError, setFilePath, setLoading, setMeta, setStretchLimits])

  const handleMeasure = useCallback(
    async (x: number, y: number) => {
      if (measureInFlight.current) return
      measureInFlight.current = true
      setScienceBusy(true)
      try {
        setPendingMeasurement(await measureTarget(currentFrameIndex, x, y))
      } catch (e) {
        setError(String(e))
      } finally {
        measureInFlight.current = false
        setScienceBusy(false)
      }
    },
    [currentFrameIndex, setError],
  )
  const confirmMeasurement = useCallback(
    async (designation: string) => {
      if (!pendingMeasurement) return
      setScienceBusy(true)
      try {
        const value = await confirmTargetMeasurement(pendingMeasurement.id, designation)
        setMeasurements((v) => [...v.filter((m) => m.id !== value.id), value])
        setPendingMeasurement(null)
        setScienceOpen(true)
      } catch (e) {
        setError(String(e))
      } finally {
        setScienceBusy(false)
      }
    },
    [pendingMeasurement, setError],
  )
  const cancelMeasurement = useCallback(async () => {
    if (!pendingMeasurement) return
    try {
      await discardTargetMeasurement(pendingMeasurement.id)
    } catch (e) {
      setError(String(e))
    } finally {
      setPendingMeasurement(null)
    }
  }, [pendingMeasurement, setError])
  const handleMpcUpdate = useCallback(async () => {
    setScienceBusy(true)
    try {
      setMpcorb(await updateMpcorb())
    } finally {
      setScienceBusy(false)
    }
  }, [])
  const handleMpcImport = useCallback(async (sourcePath: string) => {
    setScienceBusy(true)
    try {
      setMpcorb(await importMpcorb(sourcePath))
    } finally {
      setScienceBusy(false)
    }
  }, [])
  const observatory = useMemo(() => {
    const s = appConfig?.station
    const eopFresh =
      s?.eop_updated_unix != null &&
      Math.abs(APP_STARTED_UNIX_SECONDS - s.eop_updated_unix) <= 7 * 24 * 60 * 60
    return s?.longitude_deg_east != null && s.latitude_deg != null
      ? {
          longitude_deg_east: s.longitude_deg_east,
          latitude_deg: s.latitude_deg,
          altitude_m: s.altitude_m ?? 0,
          dut1_seconds: eopFresh ? (s.dut1_seconds ?? null) : null,
        }
      : undefined
  }, [appConfig])
  const handleOverlay = useCallback(async () => {
    if (!knownPrerequisites) {
      setError('显示已知目标需要整组图片都有 accepted WCS 和 UTC 曝光中点。')
      return false
    }
    const searches = frames.map((frame, frameIndex) => {
      const w = useSessionStore.getState().frameAnalyses[frameIndex]!.solution!.wcs!
      const scale = (Math.hypot(w.cd1_1, w.cd2_1) + Math.hypot(w.cd1_2, w.cd2_2)) * 1800
      return {
        frame_index: frameIndex,
        jd_utc: frame.observation_midpoint_jd!,
        center_ra_deg: w.crval1,
        center_dec_deg: w.crval2,
        radius_deg: ((Math.hypot(frame.width, frame.height) * scale) / 7200) * 1.15,
      }
    })
    setKnownSearchBusy(true)
    try {
      const result = await searchKnownObjectsBatch({
        frames: searches,
        station: observatory,
        max_results_per_frame: 1000,
      })
      setKnownObjectsByFrame(
        Object.fromEntries(result.frames.map((frame) => [frame.frame_index, frame.objects])),
      )
      const warnings = [...new Set(result.frames.flatMap((frame) => frame.warnings))]
      if (warnings.length > 0) setReductionMessage(warnings.join('；'))
      return true
    } catch (e) {
      setError(String(e))
      return false
    } finally {
      setKnownSearchBusy(false)
    }
  }, [frames, knownPrerequisites, observatory, setError])
  const handleKnownToggle = useCallback(async () => {
    if (knownVisible) {
      setKnownVisible(false)
      return
    }
    if (Object.keys(knownObjectsByFrame).length > 0) {
      setKnownVisible(true)
      return
    }
    if (await handleOverlay()) setKnownVisible(true)
  }, [handleOverlay, knownObjectsByFrame, knownVisible])
  const handleMatch = useCallback(async () => {
    const points = measurements
      .filter((m) => m.midpoint_jd != null && m.ra_deg != null && m.dec_deg != null)
      .map((m) => ({
        measurement_id: m.id,
        jd_utc: m.midpoint_jd!,
        ra_deg: m.ra_deg!,
        dec_deg: m.dec_deg!,
      }))
    setScienceBusy(true)
    try {
      const r = await matchTracklet(points, observatory)
      setReductionMessage(
        `${r.status}: ${r.reason}${r.candidates[0] ? `；最佳候选 ${r.candidates[0].designation}，最大 O-C ${r.candidates[0].max_residual_arcsec.toFixed(2)}″` : ''}`,
      )
    } catch (e) {
      setError(String(e))
    } finally {
      setScienceBusy(false)
    }
  }, [measurements, observatory, setError])
  const handleSaveConfig = useCallback(async (config: AppConfig) => {
    const result = await saveAppConfig(config)
    setAppConfig(config)
    if (result.scientific_cache_invalidated) {
      useSessionStore.getState().invalidateScience()
      setMeasurements((current) =>
        current.map((measurement) => ({
          ...measurement,
          stale: true,
          stale_reason: '科学设置已变化，需要重新归算',
        })),
      )
      setReductionMessage('科学设置已保存；旧归算与测量结果已失效，请重新归算。')
    } else {
      setReductionMessage('软件设置已保存到 config/settings.json。')
    }
  }, [])
  const handleDeleteMeasurement = useCallback(
    async (id: string) => {
      setScienceBusy(true)
      try {
        await deleteTargetMeasurement(id)
        setMeasurements((current) => current.filter((measurement) => measurement.id !== id))
      } catch (e) {
        setError(String(e))
      } finally {
        setScienceBusy(false)
      }
    },
    [setError],
  )
  const handleRenameMeasurement = useCallback(
    async (id: string, name: string) => {
      setScienceBusy(true)
      try {
        const updated = await renameTargetMeasurement(id, name)
        setMeasurements((current) =>
          current.map((measurement) => (measurement.id === id ? updated : measurement)),
        )
      } catch (e) {
        setError(String(e))
      } finally {
        setScienceBusy(false)
      }
    },
    [setError],
  )
  const openReportDialog = useCallback(() => {
    if (!appConfig) return
    setReportFormat(appConfig.report.default_format)
    setReportPreview('')
    setReportWarnings([])
    setReportError(null)
    setReportOpen(true)
  }, [appConfig])
  const closeReportDialog = useCallback(() => {
    setReportOpen(false)
    setReportWarnings([])
    setReportError(null)
  }, [])
  const handleReportExport = useCallback(async () => {
    if (!reportContext) return
    setReportBusy(true)
    setReportError(null)
    try {
      const preview = await previewReport(reportFormat, reportContext, reportObservations)
      setReportPreview(preview.content)
      setReportWarnings(preview.warnings)
      const extension = reportFormat === 'ades2022_psv' ? 'psv' : 'txt'
      const destination = await saveDialog({
        defaultPath: `skyeye-observations.${extension}`,
        filters: [
          {
            name: reportFormat === 'ades2022_psv' ? 'ADES 2022 PSV' : 'MPC 80-column',
            extensions: [extension],
          },
        ],
      })
      if (!destination) return
      await exportReport(destination, reportFormat, reportContext, reportObservations)
      setReportOpen(false)
      setReductionMessage(`观测报告已导出：${destination}`)
    } catch (e) {
      // 报告校验/写入失败留在对话框内展示，避免在报告弹窗之上再叠全局错误弹窗
      setReportError(String(e))
    } finally {
      setReportBusy(false)
    }
  }, [reportContext, reportFormat, reportObservations])

  useEffect(() => {
    if (!reportOpen || !reportContext) return
    let active = true
    setReportPreview('')
    setReportWarnings([])
    setReportError(null)
    setReportBusy(true)
    void previewReport(reportFormat, reportContext, reportObservations)
      .then((preview) => {
        if (active) {
          setReportPreview(preview.content)
          setReportWarnings(preview.warnings)
        }
      })
      .catch((error) => {
        if (active) setReportError(String(error))
      })
      .finally(() => {
        if (active) setReportBusy(false)
      })
    return () => {
      active = false
    }
  }, [
    reportOpen,
    reportFormat,
    reportContext,
    reportObservations,
    setReportPreview,
    setReportWarnings,
    setReportError,
    setReportBusy,
  ])

  const beginManualCalibration = useCallback(
    async (frameIndex: number, solveParams: SolveParams) => {
      manualFrameIndex.current = frameIndex
      const blinkState = await blinkSetFrame(frameIndex)
      setBlinkState(blinkState)

      const analysis = await getFrameAnalysis(frameIndex)
      if (!analysis.detection || !analysis.catalog) return false

      const frame = frames[frameIndex]
      if (!frame) return false
      const solvedWcs = analysis.solution?.wcs
      const solvedScale = solvedWcs
        ? (Math.hypot(solvedWcs.cd1_1, solvedWcs.cd2_1) +
            Math.hypot(solvedWcs.cd1_2, solvedWcs.cd2_2)) *
          1_800
        : undefined
      const solvedRotation = solvedWcs
        ? (Math.atan2(solvedWcs.cd2_1, solvedWcs.cd1_1) * 180) / Math.PI
        : undefined
      const solvedParity = solvedWcs
        ? solvedWcs.cd1_1 * solvedWcs.cd2_2 - solvedWcs.cd1_2 * solvedWcs.cd2_1 < 0
        : undefined
      const scale = solveParams.pixel_scale_arcsec ?? solvedScale ?? frame?.pixel_scale_arcsec
      if (scale == null || !Number.isFinite(scale)) return false

      setDetection(analysis.detection)
      setManualCalibration({
        sources: analysis.catalog.sources,
        imageCandidateLimit: Math.min(
          100,
          Math.max(50, solveParams.maximum_reference_stars ?? 100),
        ),
        seed: {
          center_ra_deg: solvedWcs?.crval1 ?? analysis.catalog.query.ra_deg,
          center_dec_deg: solvedWcs?.crval2 ?? analysis.catalog.query.dec_deg,
          pixel_scale_arcsec: scale,
          rotation_deg: solveParams.rotation_deg ?? solvedRotation ?? frame?.rotation_deg ?? 0,
          parity_flipped:
            solveParams.parity_flipped ?? solvedParity ?? frame?.parity_flipped ?? false,
          offset_x_px: solvedWcs ? solvedWcs.crpix1 - frame.width * 0.5 : 0,
          offset_y_px: solvedWcs ? solvedWcs.crpix2 - frame.height * 0.5 : 0,
          catalog_bright_limit_mag: solveParams.catalog_bright_limit_mag ?? 5,
          catalog_faint_limit_mag: Math.min(
            22,
            Math.max(20, solveParams.catalog_faint_limit_mag ?? 20),
          ),
        },
      })
      setReductionMessage(null)
      return true
    },
    [frames, setBlinkState, setDetection],
  )

  const handleReduction = useCallback(
    async (params: SolveParams = {}) => {
      const configuredScale =
        appConfig?.instrument.focal_length_mm && appConfig.instrument.pixel_width_um
          ? (206.265 * appConfig.instrument.pixel_width_um) / appConfig.instrument.focal_length_mm
          : undefined
      const configuredParity = appConfig
        ? appConfig.instrument.flip_horizontal !== appConfig.instrument.flip_vertical
        : undefined
      const effectiveParams: SolveParams = {
        pixel_scale_arcsec: configuredScale,
        rotation_deg: appConfig?.instrument.position_angle_deg,
        parity_flipped: configuredParity || undefined,
        catalog_bright_limit_mag: appConfig?.reduction.catalog_bright_limit_mag,
        catalog_faint_limit_mag: appConfig?.reduction.catalog_faint_limit_mag,
        maximum_reference_stars: appConfig?.reduction.maximum_reference_stars,
        astrometric_residual_limit_arcsec: appConfig?.reduction.astrometric_residual_limit_arcsec,
        ...params,
      }
      lastSolveParams.current = effectiveParams
      setManualCalibration(null)
      setManualQueue([])
      manualFrameIndex.current = null
      setError(null)
      setReductionMessage(null)
      setReductionProgress(null)
      setIsDetecting(true)
      try {
        setIsDetecting(false)
        setIsSolving(true)
        const batch = await startReduction(effectiveParams, (progress) => {
          setReductionProgress(progress.message)
        })
        const pendingFrames = batch.frames.filter((frame) => frame.solution.status !== 'accepted')
        const missingHintFrames = pendingFrames.filter(
          (frame) => frame.solution.failure_code === 'missing_hint',
        )
        const photometryFailures = batch.frames.filter(
          (frame) => frame.solution.status === 'accepted' && frame.photometry_error,
        )
        const automaticPhotometryFailed =
          pendingFrames.length === 0 && photometryFailures.length > 0
        if (!automaticPhotometryFailed) {
          for (const frame of batch.frames) {
            setFrameReduction(
              frame.frame_index,
              frame.detection,
              frame.solution,
              frame.photometry,
              frame.photometry_error,
            )
          }
        }
        const photometryFailureMessage =
          photometryFailures.length > 0
            ? `REFCAT2 光度定标失败（${photometryFailures.map((frame) => `第 ${frame.frame_index + 1} 帧：${frame.photometry_error}`).join('；')}）。${automaticPhotometryFailed ? '本次自动归算结果已全部丢弃，请修正配置或数据后重新归算。' : '当前仍有帧需要人工校准，中间结果仅用于继续人工校准。'}`
            : null

        if (pendingFrames.length === 0) {
          if (photometryFailureMessage) {
            setError(photometryFailureMessage)
          } else {
            const scope =
              batch.frames.length > 1 ? `整组 ${batch.frames.length} 帧归算完成` : '本帧归算完成'
            setReductionMessage(`${scope}；REFCAT2 光度定标全部完成。`)
          }
        } else if (missingHintFrames.length > 0) {
          const firstMissing = missingHintFrames[0]
          const blinkState = await blinkSetFrame(firstMissing.frame_index)
          setBlinkState(blinkState)
          const astrometryError =
            missingHintFrames.length > 1
              ? `整组有 ${missingHintFrames.length} 帧缺少归算初值。${firstMissing.solution.message}`
              : firstMissing.solution.message
          setError(
            photometryFailureMessage
              ? `${astrometryError}\n${photometryFailureMessage}`
              : astrometryError,
          )
        } else {
          const [firstPending, ...remainingFrames] = pendingFrames
          setManualQueue(remainingFrames.map((frame) => frame.frame_index))
          const manualOpened = await beginManualCalibration(
            firstPending.frame_index,
            effectiveParams,
          )
          if (manualOpened) {
            setError(photometryFailureMessage)
          } else {
            setError(
              photometryFailureMessage
                ? `${firstPending.solution.message}\n${photometryFailureMessage}`
                : firstPending.solution.message,
            )
          }
        }
      } catch (err) {
        setError(String(err))
      } finally {
        setIsDetecting(false)
        setIsSolving(false)
        setReductionProgress(null)
      }
    },
    [
      appConfig,
      beginManualCalibration,
      setBlinkState,
      setError,
      setIsDetecting,
      setIsSolving,
      setFrameReduction,
    ],
  )

  const updateManualSeed = useCallback((seed: ManualCalibrationSeed) => {
    setManualCalibration((current) => (current ? { ...current, seed } : current))
  }, [])

  const handleManualRefine = useCallback(async () => {
    if (!manualCalibration) return
    setManualBusy(true)
    setError(null)
    setReductionProgress('正在使用人工初值重新归算…')
    setIsSolving(true)
    try {
      const seed = manualCalibration.seed
      const solution = await plateSolve({
        center_ra_deg: seed.center_ra_deg,
        center_dec_deg: seed.center_dec_deg,
        pixel_scale_arcsec: seed.pixel_scale_arcsec,
        rotation_deg: seed.rotation_deg,
        parity_flipped: seed.parity_flipped,
        offset_x_px: seed.offset_x_px,
        offset_y_px: seed.offset_y_px,
        catalog_bright_limit_mag: seed.catalog_bright_limit_mag,
        catalog_faint_limit_mag: seed.catalog_faint_limit_mag,
        maximum_reference_stars: manualCalibration.imageCandidateLimit,
        accept_review: true,
      })
      setSolution(solution)
      if (!solution.success) {
        setError(`使用人工初值归算后仍未通过：${solution.message}`)
        const wcs = solution.wcs
        const frame = frames[currentFrameIndex]
        if (wcs && frame) {
          const scale =
            (Math.hypot(wcs.cd1_1, wcs.cd2_1) + Math.hypot(wcs.cd1_2, wcs.cd2_2)) * 1_800
          const determinant = wcs.cd1_1 * wcs.cd2_2 - wcs.cd1_2 * wcs.cd2_1
          setManualCalibration((current) =>
            current
              ? {
                  ...current,
                  seed: {
                    ...current.seed,
                    center_ra_deg: wcs.crval1,
                    center_dec_deg: wcs.crval2,
                    pixel_scale_arcsec: scale,
                    rotation_deg: (Math.atan2(wcs.cd2_1, wcs.cd1_1) * 180) / Math.PI,
                    parity_flipped: determinant < 0,
                    offset_x_px: wcs.crpix1 - frame.width * 0.5,
                    offset_y_px: wcs.crpix2 - frame.height * 0.5,
                  },
                }
              : current,
          )
        }
      } else {
        let photometryMessage: string | null = null
        let photometryError: string | null = null
        setReductionProgress('WCS 已通过，正在使用 REFCAT2 进行光度定标…')
        try {
          const calibrated = await calibrateFramePhotometry(currentFrameIndex)
          setMeasurements((current) =>
            current.map(
              (measurement) =>
                calibrated.measurements.find((item) => item.id === measurement.id) ?? measurement,
            ),
          )
          const analysis = await getFrameAnalysis(currentFrameIndex)
          setFrameReduction(
            currentFrameIndex,
            analysis.detection,
            solution,
            analysis.photometry,
            analysis.photometry_error,
          )
          photometryMessage = `；光度定标完成，${calibrated.matched_reference_stars} 颗参考星，ZP ${calibrated.solution.zero_point.toFixed(3)}`
        } catch (reason) {
          photometryError = `REFCAT2 光度定标失败：${String(reason)}。本帧 WCS 已保留，但在重新归算并成功完成光度定标前只能保留 flux。`
        }
        setManualCalibration(null)
        manualFrameIndex.current = null
        const [nextFrameIndex, ...remainingFrames] = manualQueue
        if (nextFrameIndex == null) {
          if (photometryError) {
            setError(photometryError)
          } else {
            setReductionMessage(
              frames.length > 1
                ? `整组 ${frames.length} 帧归算完成${photometryMessage ?? ''}。`
                : `${solution.message}${photometryMessage ?? ''}`,
            )
          }
        } else {
          setManualQueue(remainingFrames)
          const manualOpened = await beginManualCalibration(nextFrameIndex, lastSolveParams.current)
          if (!manualOpened) {
            const nextFrameError = '下一帧无法直接进入人工对齐，请补充归算初值。'
            setError(photometryError ? `${nextFrameError}\n${photometryError}` : nextFrameError)
          } else if (photometryError) {
            setError(photometryError)
          }
        }
      }
    } catch (err) {
      setError(String(err))
    } finally {
      setManualBusy(false)
      setIsSolving(false)
      setReductionProgress(null)
    }
  }, [
    beginManualCalibration,
    currentFrameIndex,
    frames,
    manualCalibration,
    manualQueue,
    setError,
    setFrameReduction,
    setIsSolving,
    setSolution,
  ])

  useEffect(() => {
    if (manualCalibration && manualFrameIndex.current !== currentFrameIndex) {
      setManualCalibration(null)
      setManualQueue([])
      manualFrameIndex.current = null
    }
  }, [currentFrameIndex, manualCalibration])

  return (
    <TooltipProvider delayDuration={400}>
      <div className="relative h-screen w-screen flex flex-col bg-sky-canvas-soft-2 overflow-hidden">
        <TitleBar
          settingsOpen={settingsOpen}
          onSettings={() => setSettingsOpen((value) => !value)}
        />

        {/* Toolbar */}
        <Toolbar aria-label="图像操作">
          <IconButton
            variant="tool"
            onClick={handleOpen}
            disabled={isLoading}
            label="打开图像"
            tooltip={isLoading ? '正在读取图像' : '打开图像'}
          >
            <FolderOpen size={14} />
          </IconButton>
          <IconButton
            variant="ghost"
            onClick={handleCloseAllImages}
            disabled={
              frames.length === 0 ||
              isLoading ||
              isDetecting ||
              isSolving ||
              knownSearchBusy ||
              scienceBusy
            }
            label="关闭所有图像"
            tooltip={frames.length === 0 ? '当前没有可关闭的图像' : '关闭所有图像'}
          >
            <ImageOff size={14} />
          </IconButton>

          <div className="h-4 w-px bg-sky-hairline" aria-hidden="true" />
          <IconButton
            variant="ghost"
            onClick={() => handleReduction()}
            disabled={frames.length === 0 || isDetecting || isSolving || isPlaying}
            label="归算"
            tooltip={
              isPlaying
                ? '闪图播放中，暂停后才能归算'
                : frames.length === 0
                  ? '请先打开 FITS 文件'
                  : '执行图像归算'
            }
          >
            <Orbit size={14} />
          </IconButton>
          <IconButton
            variant={knownVisible ? 'tool' : 'ghost'}
            onClick={handleKnownToggle}
            disabled={knownSearchBusy || !knownPrerequisites || isPlaying}
            aria-pressed={knownVisible}
            label={knownVisible ? '隐藏已知目标' : '显示已知目标'}
            tooltip={
              isPlaying
                ? '闪图播放中，暂停后才能显示已知目标'
                : !mpcorb
                  ? '请先下载 MPCORB'
                  : !knownPrerequisites
                    ? '需要整组图片都完成 WCS 归算并具有 UTC 曝光中点'
                    : knownVisible
                      ? `隐藏已知目标（${knownObjectCount} 个）`
                      : '显示已知目标'
            }
          >
            {knownVisible ? <EyeOff size={14} /> : <Eye size={14} />}
          </IconButton>

          <div className="h-4 w-px bg-sky-hairline" aria-hidden="true" />
          <IconButton
            variant="tool"
            className={scienceOpen ? 'bg-sky-control-hover text-sky-ink' : ''}
            onClick={() => setScienceOpen((open) => !open)}
            disabled={measurements.length === 0}
            aria-pressed={scienceOpen}
            label="可疑目标列表"
            tooltip={measurements.length === 0 ? '当前没有可疑目标' : '打开或关闭可疑目标列表'}
          >
            <ListChecks size={14} />
          </IconButton>
          <IconButton
            variant="ghost"
            onClick={openReportDialog}
            disabled={!appConfig || reportMeasurements.length === 0}
            label="导出报告"
            tooltip={
              reportMeasurements.length === 0
                ? '至少需要一条可疑目标记录'
                : '选择 ADES PSV 或 MPC 80-column 格式导出'
            }
          >
            <FileOutput size={14} />
          </IconButton>
        </Toolbar>

        {error && (
          <MessageDialog
            tone="error"
            title="操作未完成"
            message={error}
            onClose={() => setError(null)}
          />
        )}
        {!error && reductionMessage && (
          <MessageDialog
            tone="success"
            title="归算完成"
            message={reductionMessage}
            onClose={() => setReductionMessage(null)}
          />
        )}

        {/* Main Area */}
        <div className="flex-1 flex min-h-0">
          <div className="flex-1 relative">
            <FITSViewer
              ref={viewerRef}
              stretchMode={stretchMode}
              inverted={inverted}
              manualCalibration={manualCalibration}
              onManualOffsetChange={(x, y) => {
                if (!manualCalibration) return
                updateManualSeed({
                  ...manualCalibration.seed,
                  offset_x_px: x,
                  offset_y_px: y,
                })
              }}
              measureEnabled={canMeasure && !isPlaying}
              onMeasure={handleMeasure}
              measurements={measurements}
              knownObjects={knownVisible ? currentKnownObjects : []}
            />
            {scienceOpen && (
              <SciencePanel
                measurements={measurements}
                canMatch={Boolean(mpcorb)}
                busy={scienceBusy}
                onClose={() => setScienceOpen(false)}
                onMatch={handleMatch}
                onDelete={handleDeleteMeasurement}
                onRename={handleRenameMeasurement}
              />
            )}
          </div>
          {frames.length > 0 && (
            <DisplaySidebar
              stretchMode={stretchMode}
              onStretchModeChange={(mode) => {
                setStretchMode(mode)
              }}
              inverted={inverted}
              onInvertedChange={(nextInverted) => {
                setInverted(nextInverted)
              }}
              onFitView={requestFit}
            />
          )}
        </div>

        {manualCalibration && (
          <ManualCalibrationPanel
            calibration={manualCalibration}
            approximateMatches={approximateManualMatches}
            busy={manualBusy}
            onChange={updateManualSeed}
            onApply={handleManualRefine}
            onClose={() => {
              setManualCalibration(null)
              setManualQueue([])
              manualFrameIndex.current = null
            }}
          />
        )}

        {settingsOpen && appConfig && (
          <SettingsDialog
            config={appConfig}
            mpcorb={mpcorb}
            mpcorbBusy={scienceBusy}
            onClose={() => setSettingsOpen(false)}
            onSave={handleSaveConfig}
            onUpdateMpcorb={handleMpcUpdate}
            onImportMpcorb={handleMpcImport}
          />
        )}

        {pendingMeasurement && (
          <SuspiciousTargetDialog
            measurement={pendingMeasurement}
            pixels={framePixels[pendingMeasurement.frame_index]?.pixels}
            width={frames[pendingMeasurement.frame_index]?.width ?? frameWidth}
            height={frames[pendingMeasurement.frame_index]?.height ?? frameHeight}
            busy={scienceBusy}
            onConfirm={confirmMeasurement}
            onCancel={cancelMeasurement}
          />
        )}

        {reportOpen && (
          <ReportExportDialog
            format={reportFormat}
            preview={reportPreview}
            warnings={reportWarnings}
            error={reportError}
            busy={reportBusy}
            onFormatChange={setReportFormat}
            onExport={handleReportExport}
            onClose={closeReportDialog}
          />
        )}

        {isLoading && (
          <OperationDialog title="正在读取 FITS" message={loadingProgress ?? '正在读取图像信息…'} />
        )}
        {!isLoading && (isDetecting || isSolving) && (
          <OperationDialog
            title="正在归算"
            message={
              reductionProgress ?? (isDetecting ? '正在提取星点…' : '正在匹配星表并拟合 WCS…')
            }
          />
        )}
        {knownSearchBusy && (
          <OperationDialog
            title="正在解析已知目标"
            message={`正在解析本地 MPCORB 并标识已知小行星位置…`}
          />
        )}
      </div>
    </TooltipProvider>
  )
}

export default App
