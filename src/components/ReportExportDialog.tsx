import type { ReportFormat } from '../lib/tauri'
import { Button } from './ui/button'

interface Props {
  format: ReportFormat
  preview: string
  warnings: string[]
  error: string | null
  busy: boolean
  onFormatChange: (format: ReportFormat) => void
  onExport: () => void
  onClose: () => void
}

export function ReportExportDialog({
  format,
  preview,
  warnings,
  error,
  busy,
  onFormatChange,
  onExport,
  onClose,
}: Props) {
  return (
    <div
      className="fixed inset-x-0 bottom-0 top-9 z-[120] grid place-items-center bg-black/50 px-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="report-export-title"
    >
      <section className="flex h-[min(620px,calc(100vh-68px))] w-[min(760px,calc(100vw-32px))] flex-col overflow-hidden rounded-lg border border-sky-hairline bg-sky-canvas-soft">
        <header className="flex shrink-0 items-center justify-between border-b border-sky-hairline px-5 py-3">
          <div>
            <h2 id="report-export-title" className="text-body-sm font-medium text-sky-ink">
              导出观测报告
            </h2>
            <p className="mt-0.5 text-label text-sky-mute">
              同一组可疑目标，可在导出时选择报告格式
            </p>
          </div>
          <Button variant="ghost" size="sm" onClick={onClose} disabled={busy}>
            关闭
          </Button>
        </header>
        <div className="flex shrink-0 items-center gap-2 border-b border-sky-hairline px-5 py-3">
          <Button
            size="sm"
            variant={format === 'ades2022_psv' ? 'active' : 'ghost'}
            onClick={() => onFormatChange('ades2022_psv')}
            disabled={busy}
          >
            ADES 2022 PSV
          </Button>
          <Button
            size="sm"
            variant={format === 'mpc1992_80_column' ? 'active' : 'ghost'}
            onClick={() => onFormatChange('mpc1992_80_column')}
            disabled={busy}
          >
            MPC 80-column
          </Button>
          <span className="ml-auto text-caption-mono text-sky-mute">
            {format === 'ades2022_psv' ? '.psv' : '.txt'}
          </span>
        </div>
        {!error && warnings.length > 0 && (
          <div
            role="status"
            className="max-h-32 shrink-0 overflow-auto border-b border-sky-hairline bg-sky-canvas px-5 py-2.5"
          >
            <p className="text-label font-medium text-sky-warning">
              {warnings.length} 条提示 · 不阻断导出
            </p>
            <ul className="mt-1 space-y-0.5">
              {warnings.map((warning, index) => (
                <li key={index} className="text-caption-mono leading-5 text-sky-body">
                  {warning}
                </li>
              ))}
            </ul>
          </div>
        )}
        <div className="min-h-0 flex-1 p-4">
          {error ? (
            <div
              role="alert"
              className="h-full overflow-auto rounded-md border border-sky-error bg-sky-canvas px-4 py-3"
            >
              <p className="text-body-sm font-medium text-sky-error">校验未通过</p>
              <p className="mt-2 whitespace-pre-wrap text-caption-mono leading-5 text-sky-body">
                {error}
              </p>
            </div>
          ) : (
            <pre className="h-full overflow-auto rounded-md border border-sky-hairline-strong bg-sky-canvas px-4 py-3 text-caption-mono leading-5 text-sky-body">
              {preview || (busy ? '正在生成预览…' : '当前测量无法生成预览。')}
            </pre>
          )}
        </div>
        <footer className="flex shrink-0 justify-end gap-2 border-t border-sky-hairline px-5 py-3">
          <span className="mr-auto self-center text-label text-sky-mute">
            打开窗口或切换格式时自动校验并刷新
          </span>
          <Button variant="primary" size="sm" onClick={onExport} disabled={busy || error != null}>
            {busy ? '处理中…' : '导出文件'}
          </Button>
        </footer>
      </section>
    </div>
  )
}
