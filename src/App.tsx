import { lazy, Suspense, useState, useEffect, useRef, useCallback } from 'react'
import type { PointerEvent as ReactPointerEvent } from 'react'
import { Cpu, Loader2, Sparkles, Copy, Check, Wand2, RotateCcw, X } from 'lucide-react'
import { api, type Settings } from './api/tauri'
import { i18n, type Lang } from './settings/i18n'
import { copyToClipboard } from './utils/clipboard'
import './index.css'

const Settings = lazy(() => import('./Settings'))
const Lens = lazy(() => import('./Lens'))

type TranslationMethod = NonNullable<Settings['screenshotTranslation']['translationMethod']>

const TRANSLATOR_WINDOW_W = 600
const TRANSLATOR_WINDOW_H = 420
const PROMPT_OPTIMIZER_WINDOW_W = 680
const PROMPT_OPTIMIZER_WINDOW_H = 540
const SETTINGS_WINDOW_W = 640
const SETTINGS_WINDOW_H = 520
const PANE_RATIO_MIN = 0.24
const PANE_RATIO_MAX = 0.76

function clampPaneRatio(value: number) {
  if (!Number.isFinite(value)) return 0.5
  return Math.min(PANE_RATIO_MAX, Math.max(PANE_RATIO_MIN, value))
}

function loadPaneRatio(storageKey: string, fallback: number) {
  try {
    const raw = window.localStorage.getItem(storageKey)
    if (!raw) return clampPaneRatio(fallback)
    return clampPaneRatio(Number(raw))
  } catch {
    return clampPaneRatio(fallback)
  }
}

function useRememberedPaneRatio(storageKey: string, fallback = 0.5) {
  const [ratio, setRatio] = useState(() => loadPaneRatio(storageKey, fallback))

  const saveRatio = useCallback((next: number) => {
    const clamped = clampPaneRatio(next)
    setRatio(clamped)
    try {
      window.localStorage.setItem(storageKey, clamped.toFixed(4))
    } catch {
      // localStorage can be unavailable in unusual webview modes; resizing should still work.
    }
  }, [storageKey])

  const handlePointerDown = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault()
    event.stopPropagation()

    const container = event.currentTarget.parentElement
    const rect = container?.getBoundingClientRect()
    if (!rect || rect.height <= 0) return

    const previousUserSelect = document.body.style.userSelect
    const previousCursor = document.body.style.cursor
    document.body.style.userSelect = 'none'
    document.body.style.cursor = 'row-resize'

    const updateFromClientY = (clientY: number) => {
      saveRatio((clientY - rect.top) / rect.height)
    }
    updateFromClientY(event.clientY)

    const handlePointerMove = (moveEvent: PointerEvent) => {
      moveEvent.preventDefault()
      updateFromClientY(moveEvent.clientY)
    }

    const finishDrag = () => {
      document.removeEventListener('pointermove', handlePointerMove)
      document.removeEventListener('pointerup', finishDrag)
      document.removeEventListener('pointercancel', finishDrag)
      document.body.style.userSelect = previousUserSelect
      document.body.style.cursor = previousCursor
    }

    document.addEventListener('pointermove', handlePointerMove)
    document.addEventListener('pointerup', finishDrag, { once: true })
    document.addEventListener('pointercancel', finishDrag, { once: true })
  }, [saveRatio])

  return { ratio, handlePointerDown }
}

function PaneResizeHandle({
  onPointerDown,
}: {
  onPointerDown: (event: ReactPointerEvent<HTMLDivElement>) => void
}) {
  return (
    <div
      role="separator"
      aria-orientation="horizontal"
      aria-label="调整上下区域大小"
      className="group/resize relative -my-1.5 h-3 shrink-0 cursor-row-resize"
      data-tauri-drag-region="false"
      onPointerDown={onPointerDown}
    >
      <div className="absolute left-0 right-0 top-1/2 h-px -translate-y-1/2 bg-black/[0.05] transition-colors group-hover/resize:bg-black/[0.12] dark:bg-white/[0.06] dark:group-hover/resize:bg-white/[0.16]" />
      <div className="absolute left-1/2 top-1/2 h-1 w-10 -translate-x-1/2 -translate-y-1/2 rounded-full bg-neutral-300/70 opacity-0 transition-opacity group-hover/resize:opacity-100 dark:bg-neutral-600/80" />
    </div>
  )
}

function Translator({
  translateSource,
  lang,
}: {
  translateSource: string
  lang: Lang
}) {
  const [input, setInput] = useState('')
  const [result, setResult] = useState('')
  const [loading, setLoading] = useState(false)
  const [translationMethod, setTranslationMethod] = useState<TranslationMethod>('ai')
  const [methodSwitching, setMethodSwitching] = useState(false)
  const resultRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const translateSeq = useRef(0)
  const skipNextDebouncedInputRef = useRef<string | null>(null)
  const t = i18n[lang]
  const {
    ratio: paneRatio,
    handlePointerDown: handlePaneResizePointerDown,
  } = useRememberedPaneRatio('kivio.translatorPaneRatio', 0.5)
  const translationMethodOptions: { value: TranslationMethod; label: string }[] = [
    { value: 'ai', label: t.screenshotTranslationAI },
    { value: 'google', label: t.screenshotTranslationGoogle },
    { value: 'baidu', label: t.screenshotTranslationBaidu },
    { value: 'tencent', label: t.screenshotTranslationTencent },
    { value: 'bing', label: t.screenshotTranslationBing },
    { value: 'bing2', label: t.screenshotTranslationBing2 },
    { value: 'yandex', label: t.screenshotTranslationYandex },
    { value: 'caiyun2', label: t.screenshotTranslationCaiyun2 },
    { value: 'microsoft', label: t.screenshotTranslationMicrosoft },
  ]
  const methodSelectClass = 'ml-auto shrink-0 h-5 max-w-[122px] rounded-md border border-black/[0.06] bg-white/80 px-1.5 text-[10.5px] font-medium text-neutral-500 outline-none hover:text-neutral-700 hover:border-black/[0.12] disabled:opacity-60 dark:border-white/[0.08] dark:bg-neutral-900/70 dark:text-neutral-400 dark:hover:text-neutral-100 dark:hover:border-white/[0.14]'

  const runTranslationNow = useCallback(async (text: string) => {
    const seq = ++translateSeq.current
    const trimmed = text.trim()
    if (!trimmed) {
      setResult('')
      setLoading(false)
      return
    }

    setLoading(true)
    try {
      const translated = await api.translateText(text)
      if (seq !== translateSeq.current) return
      setResult(translated)
    } catch (e) {
      if (seq !== translateSeq.current) return
      console.error(e)
      setResult(typeof e === 'string' ? e : (e as Error).message || 'Error')
    } finally {
      if (seq === translateSeq.current) setLoading(false)
    }
  }, [])

  useEffect(() => {
    let active = true
    api.getSettings()
      .then((settings) => {
        if (!active) return
        setTranslationMethod(settings.screenshotTranslation?.translationMethod || 'ai')
      })
      .catch(err => console.error('[Translator] Failed to load translation method:', err))
    return () => { active = false }
  }, [])

  const handleTranslationMethodChange = useCallback(async (value: string) => {
    const method = value as TranslationMethod
    if (method === translationMethod || methodSwitching) return
    setMethodSwitching(true)
    const previous = translationMethod
    setTranslationMethod(method)
    translateSeq.current += 1
    setResult('')
    setLoading(!!input.trim())
    try {
      const settings = await api.getSettings()
      await api.saveSettings({
        ...settings,
        screenshotTranslation: {
          ...settings.screenshotTranslation,
          translationMethod: method,
        },
      })
      if (input.trim()) {
        await runTranslationNow(input)
      } else {
        setLoading(false)
      }
    } catch (err) {
      setTranslationMethod(previous)
      setLoading(false)
      console.error('[Translator] Failed to switch translation method:', err)
      setResult(err instanceof Error ? err.message : String(err))
    } finally {
      setMethodSwitching(false)
    }
  }, [input, methodSwitching, runTranslationNow, translationMethod])

  useEffect(() => {
    const trimmed = input.trim()
    if (!trimmed) {
      translateSeq.current += 1
      setResult('')
      setLoading(false)
      return
    }
    if (skipNextDebouncedInputRef.current === input) {
      skipNextDebouncedInputRef.current = null
      return
    }

    const seq = ++translateSeq.current
    const timer = setTimeout(async () => {
      if (seq !== translateSeq.current) return
      setLoading(true)
      try {
        const translated = await api.translateText(input)
        if (seq !== translateSeq.current) return
        setResult(translated)
      } catch (e) {
        if (seq !== translateSeq.current) return
        console.error(e)
        setResult(typeof e === 'string' ? e : (e as Error).message || 'Error')
      } finally {
        if (seq === translateSeq.current) setLoading(false)
      }
    }, 600)
    return () => clearTimeout(timer)
  }, [input])

  const applyPendingTranslatorSelection = useCallback(async () => {
    try {
      const text = await api.takeTranslatorSelection()
      if (!text.trim()) return
      skipNextDebouncedInputRef.current = text
      setInput(text)
      setResult('')
      inputRef.current?.focus({ preventScroll: true })
      void runTranslationNow(text)
    } catch (err) {
      console.error('[Translator] Failed to take selected text:', err)
    }
  }, [runTranslationNow])

  useEffect(() => {
    void applyPendingTranslatorSelection()
    const onFocus = () => { void applyPendingTranslatorSelection() }
    const onHashChange = () => { void applyPendingTranslatorSelection() }
    window.addEventListener('focus', onFocus)
    window.addEventListener('hashchange', onHashChange)
    return () => {
      window.removeEventListener('focus', onFocus)
      window.removeEventListener('hashchange', onHashChange)
    }
  }, [applyPendingTranslatorSelection])

  const closeTranslatorWindow = useCallback(async () => {
    try {
      await api.closeWindow()
    } catch (err) {
      console.error('[Translator] Failed to hide window:', err)
    }
  }, [])

  useEffect(() => {
    const handler = async (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        await closeTranslatorWindow()
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [closeTranslatorWindow])

  useEffect(() => {
    if (resultRef.current) {
      resultRef.current.scrollTop = resultRef.current.scrollHeight
    }
  }, [result])

  // IME 合成中（中/日/韩输入法选词按回车）不要触发：isComposing 是组合事件官方标志，
  // keyCode === 229 是浏览器在 IME 拦截 keydown 时的兜底信号，两个条件并查更稳。
  const handleKeyDown = async (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key !== 'Enter') return
    if (e.nativeEvent.isComposing || e.keyCode === 229) return
    if (!e.ctrlKey && !e.metaKey) return
    e.preventDefault()
    const textToCommit = result || input
    if (!textToCommit.trim()) return
    await api.commitTranslation(textToCommit)
    setInput('')
    setResult('')
  }

  return (
    <div className="window-container">
      <div className="window-frosted h-full w-full flex flex-col select-none overflow-hidden relative group">
        <div
          className="absolute top-0 left-0 right-0 h-8 z-10"
          data-tauri-drag-region
        />

        <button
          onClick={() => void closeTranslatorWindow()}
          className="absolute top-1.5 right-2 z-20 p-1 text-neutral-400 hover:text-neutral-700 dark:text-neutral-500 dark:hover:text-neutral-200 rounded-md hover:bg-black/5 dark:hover:bg-white/10 opacity-60 hover:opacity-100 transition-all duration-150"
          title={t.shotClose}
          aria-label={t.shotClose}
          data-tauri-drag-region="false"
        >
          <X size={13} strokeWidth={1.9} />
        </button>

        <div className="relative z-0 flex h-full min-h-0 flex-col px-4 pt-4 pb-3">
          <div className="h-5 shrink-0" data-tauri-drag-region />

          <div className="flex min-h-0 flex-1 flex-col">
            <section
              className="flex min-h-0 flex-col"
              style={{ flex: `${paneRatio} 1 0%` }}
            >
              <div className="mb-1.5 flex items-center justify-between">
                <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-neutral-400 dark:text-neutral-500">
                  {t.shotOriginal}
                </span>
              </div>
              <textarea
                ref={inputRef}
                autoFocus
                spellCheck={false}
                className="min-h-0 flex-1 w-full resize-none px-3 py-2.5 bg-white/70 dark:bg-neutral-800/40 ring-1 ring-black/[0.05] dark:ring-white/[0.06] rounded-xl text-[14px] leading-[1.48] text-neutral-900 dark:text-white placeholder-neutral-400 dark:placeholder-neutral-500 focus:outline-none focus:ring-black/[0.12] dark:focus:ring-white/[0.18] focus:bg-white dark:focus:bg-neutral-800/70 transition-all custom-scrollbar select-text"
                placeholder={t.translatorPlaceholder}
                value={input}
                onChange={(e) => setInput(e.target.value)}
                onKeyDown={handleKeyDown}
              />
            </section>

            <PaneResizeHandle onPointerDown={handlePaneResizePointerDown} />

            <section
              className="flex min-h-0 flex-col"
              style={{ flex: `${1 - paneRatio} 1 0%` }}
            >
              <div className="mb-1.5 flex items-center gap-1.5">
                <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-neutral-400 dark:text-neutral-500">
                  {t.shotTranslated}
                </span>
                {(loading || methodSwitching) && (
                  <span className="flex items-center gap-1 text-[10.5px] text-neutral-400 dark:text-neutral-500">
                    <Loader2 size={10} className="animate-spin" />
                    {t.translatorTranslating}
                  </span>
                )}
                <select
                  value={translationMethod}
                  disabled={methodSwitching}
                  onChange={(e) => void handleTranslationMethodChange(e.target.value)}
                  title={t.screenshotTranslationMethod}
                  aria-label={t.screenshotTranslationMethod}
                  className={methodSelectClass}
                >
                  {translationMethodOptions.map(option => (
                    <option key={option.value} value={option.value}>{option.label}</option>
                  ))}
                </select>
              </div>
              <div
                ref={resultRef}
                className="min-h-0 flex-1 px-3 py-2.5 rounded-xl overflow-y-auto custom-scrollbar bg-gradient-to-br from-neutral-100/90 to-neutral-50/80 dark:from-neutral-800/70 dark:to-neutral-800/40 ring-1 ring-black/[0.04] dark:ring-white/[0.06] shadow-sm"
              >
                {result ? (
                  <p className="text-neutral-800 dark:text-neutral-100 text-[14px] font-normal select-text leading-[1.55] whitespace-pre-wrap break-words">
                    {result}
                  </p>
                ) : loading ? (
                  <div className="space-y-2 py-0.5">
                    <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite]" />
                    <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite] w-[78%]" />
                  </div>
                ) : null}
              </div>
            </section>
          </div>

          <div className="mt-2.5 flex shrink-0 justify-between items-center text-[10px] text-neutral-400 dark:text-neutral-500">
            <div className="flex items-center gap-2">
              <span>{t.translatorHintEnter}</span>
              <span>{t.translatorHintEsc}</span>
            </div>
            {translateSource && (
              <span className="flex items-center gap-1 opacity-70 max-w-[220px] truncate">
                <Cpu size={9} strokeWidth={1.5} className="shrink-0" />
                <span className="truncate">{translateSource}</span>
              </span>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

function PromptOptimizer({
  translateSource,
  lang,
}: {
  translateSource: string
  lang: Lang
}) {
  const [original, setOriginal] = useState('')
  const [optimized, setOptimized] = useState('')
  const [loading, setLoading] = useState(false)
  const [copied, setCopied] = useState(false)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const outputRef = useRef<HTMLTextAreaElement>(null)
  const optimizeSeq = useRef(0)
  const copiedTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const t = i18n[lang]
  const {
    ratio: paneRatio,
    handlePointerDown: handlePaneResizePointerDown,
  } = useRememberedPaneRatio('kivio.promptOptimizerPaneRatio', 0.48)

  const runOptimize = useCallback(async (text = original) => {
    const seq = ++optimizeSeq.current
    const source = text.trim()
    if (!source) {
      setOptimized('')
      setLoading(false)
      return
    }

    setLoading(true)
    setOptimized('')
    try {
      const result = await api.optimizePrompt(text)
      if (seq !== optimizeSeq.current) return
      setOptimized(result)
    } catch (err) {
      if (seq !== optimizeSeq.current) return
      console.error('[PromptOptimizer] Failed to optimize prompt:', err)
      setOptimized(err instanceof Error ? err.message : String(err))
    } finally {
      if (seq === optimizeSeq.current) setLoading(false)
    }
  }, [original])

  useEffect(() => {
    inputRef.current?.focus({ preventScroll: true })
  }, [])

  const closePromptOptimizerWindow = useCallback(async () => {
    try {
      await api.closeWindow()
    } catch (err) {
      console.error('[PromptOptimizer] Failed to hide window:', err)
    }
  }, [])

  useEffect(() => {
    const handler = async (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        await closePromptOptimizerWindow()
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [closePromptOptimizerWindow])

  useEffect(() => {
    if (outputRef.current) outputRef.current.scrollTop = outputRef.current.scrollHeight
  }, [optimized])

  useEffect(() => () => {
    if (copiedTimerRef.current) clearTimeout(copiedTimerRef.current)
  }, [])

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key !== 'Enter') return
    if (e.nativeEvent.isComposing || e.keyCode === 229) return
    if (!e.ctrlKey && !e.metaKey) return
    e.preventDefault()
    void runOptimize()
  }

  const copyOptimized = async () => {
    if (!optimized.trim()) return
    const ok = await copyToClipboard(optimized)
    if (!ok) return
    setCopied(true)
    if (copiedTimerRef.current) clearTimeout(copiedTimerRef.current)
    copiedTimerRef.current = setTimeout(() => setCopied(false), 1800)
  }

  const applyOptimized = () => {
    if (!optimized.trim()) return
    setOriginal(optimized)
    inputRef.current?.focus({ preventScroll: true })
  }

  return (
    <div className="window-container">
      <div className="window-frosted h-full w-full flex flex-col select-none overflow-hidden relative group">
        <div className="absolute top-0 left-0 right-0 h-8 z-10" data-tauri-drag-region />
        <button
          onClick={() => void closePromptOptimizerWindow()}
          className="absolute top-1.5 right-2 z-20 p-1 text-neutral-400 hover:text-neutral-700 dark:text-neutral-500 dark:hover:text-neutral-200 rounded-md hover:bg-black/5 dark:hover:bg-white/10 opacity-60 hover:opacity-100 transition-all duration-150"
          title={t.shotClose}
          aria-label={t.shotClose}
          data-tauri-drag-region="false"
        >
          <X size={13} strokeWidth={1.9} />
        </button>

        <div className="relative z-0 flex h-full min-h-0 flex-col px-4 pt-4 pb-3">
          <div className="h-5 shrink-0 flex items-center gap-1.5 text-[11px] font-semibold text-neutral-500 dark:text-neutral-400" data-tauri-drag-region>
            <Sparkles size={12} strokeWidth={1.8} />
            <span>{t.promptOptimizerTitle}</span>
          </div>

          <div className="flex min-h-0 flex-1 flex-col">
            <section
              className="flex min-h-0 flex-col"
              style={{ flex: `${paneRatio} 1 0%` }}
            >
              <div className="mb-1.5 flex items-center justify-between">
                <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-neutral-400 dark:text-neutral-500">
                  {t.promptOptimizerOriginal}
                </span>
                <span className="text-[10px] text-neutral-400 dark:text-neutral-500">{t.promptOptimizerHint}</span>
              </div>
              <textarea
                ref={inputRef}
                spellCheck={false}
                className="min-h-0 flex-1 w-full resize-none px-3 py-2.5 bg-white/70 dark:bg-neutral-800/40 ring-1 ring-black/[0.05] dark:ring-white/[0.06] rounded-xl text-[13.5px] leading-[1.52] text-neutral-900 dark:text-white placeholder-neutral-400 dark:placeholder-neutral-500 focus:outline-none focus:ring-black/[0.12] dark:focus:ring-white/[0.18] focus:bg-white dark:focus:bg-neutral-800/70 transition-all custom-scrollbar select-text"
                placeholder={t.promptOptimizerPlaceholder}
                value={original}
                onChange={(e) => setOriginal(e.target.value)}
                onKeyDown={handleKeyDown}
              />
            </section>

            <PaneResizeHandle onPointerDown={handlePaneResizePointerDown} />

            <section
              className="flex min-h-0 flex-col"
              style={{ flex: `${1 - paneRatio} 1 0%` }}
            >
              <div className="mb-1.5 flex items-center gap-1.5">
                <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-neutral-400 dark:text-neutral-500">
                  {t.promptOptimizerWorkspace}
                </span>
                {loading && (
                  <span className="flex items-center gap-1 text-[10.5px] text-neutral-400 dark:text-neutral-500">
                    <Loader2 size={10} className="animate-spin" />
                    {t.promptOptimizerOptimizing}
                  </span>
                )}
                <div className="ml-auto flex items-center gap-1">
                  <button
                    type="button"
                    onClick={() => void runOptimize()}
                    disabled={loading || !original.trim()}
                    className={`h-6 px-2 rounded-md flex items-center gap-1 text-[11px] font-medium transition-colors ${
                      !loading && original.trim()
                        ? 'bg-[#2563eb] text-white hover:bg-[#1d4ed8]'
                        : 'bg-neutral-200 dark:bg-neutral-800 text-neutral-400 cursor-not-allowed'
                    }`}
                    data-tauri-drag-region="false"
                  >
                    {loading ? <Loader2 size={11} className="animate-spin" /> : <Wand2 size={11} />}
                    {t.promptOptimizerOptimize}
                  </button>
                  <button
                    type="button"
                    onClick={applyOptimized}
                    disabled={!optimized.trim()}
                    className="h-6 px-2 rounded-md flex items-center gap-1 text-[11px] font-medium text-neutral-500 hover:text-neutral-800 dark:text-neutral-400 dark:hover:text-neutral-100 hover:bg-black/[0.05] dark:hover:bg-white/[0.08] disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
                    data-tauri-drag-region="false"
                  >
                    <RotateCcw size={11} />
                    {t.promptOptimizerApply}
                  </button>
                  <button
                    type="button"
                    onClick={() => void copyOptimized()}
                    disabled={!optimized.trim()}
                    className="h-6 px-2 rounded-md flex items-center gap-1 text-[11px] font-medium text-neutral-500 hover:text-neutral-800 dark:text-neutral-400 dark:hover:text-neutral-100 hover:bg-black/[0.05] dark:hover:bg-white/[0.08] disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
                    data-tauri-drag-region="false"
                  >
                    {copied ? <Check size={11} /> : <Copy size={11} />}
                    {copied ? t.promptOptimizerCopied : t.promptOptimizerCopy}
                  </button>
                </div>
              </div>
              <div className="relative min-h-0 flex-1">
                {loading && !optimized ? (
                  <div className="h-full min-h-0 w-full rounded-xl px-3 py-2.5 bg-gradient-to-br from-neutral-100/90 to-neutral-50/80 dark:from-neutral-800/70 dark:to-neutral-800/40 ring-1 ring-black/[0.04] dark:ring-white/[0.06] shadow-sm">
                    <div className="space-y-2 py-0.5">
                      <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite]" />
                      <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite] w-[82%]" />
                      <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite] w-[68%]" />
                    </div>
                  </div>
                ) : (
                  <textarea
                    ref={outputRef}
                    spellCheck={false}
                    className="h-full min-h-0 w-full resize-none px-3 py-2.5 rounded-xl overflow-y-auto custom-scrollbar bg-gradient-to-br from-neutral-100/90 to-neutral-50/80 dark:from-neutral-800/70 dark:to-neutral-800/40 ring-1 ring-black/[0.04] dark:ring-white/[0.06] shadow-sm text-neutral-800 dark:text-neutral-100 text-[13.5px] font-normal select-text leading-[1.58] placeholder-neutral-400 dark:placeholder-neutral-500 focus:outline-none focus:ring-black/[0.12] dark:focus:ring-white/[0.18]"
                    placeholder={t.promptOptimizerEmpty}
                    value={optimized}
                    onChange={(e) => setOptimized(e.target.value)}
                  />
                )}
              </div>
            </section>
          </div>

          <div className="mt-2.5 flex shrink-0 justify-end items-center text-[10px] text-neutral-400 dark:text-neutral-500">
            {translateSource && (
              <span className="flex items-center gap-1 opacity-70 max-w-[260px] truncate">
                <Cpu size={9} strokeWidth={1.5} className="shrink-0" />
                <span className="truncate">{translateSource}</span>
              </span>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

function App() {
  const getMode = () => {
    const urlParams = new URLSearchParams(window.location.search)
    const hash = window.location.hash.replace('#', '')
    return urlParams.get('mode') || hash.split('?')[0] || ''
  }

  const [mode, setMode] = useState(getMode)
  const [themeMode, setThemeMode] = useState<'system' | 'light' | 'dark'>('system')
  const [translateSource, setTranslateSource] = useState<string>('')
  const [lang, setLang] = useState<Lang>('zh')

  const applyTheme = async () => {
    const settings = await api.getSettings()
    const nextMode = (settings.theme || 'system') as 'system' | 'light' | 'dark'
    setThemeMode(nextMode)
    const isDark = nextMode === 'dark' || (nextMode === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches)
    if (isDark) {
      document.documentElement.classList.add('dark')
    } else {
      document.documentElement.classList.remove('dark')
    }
    setTranslateSource(settings.translatorModel || 'AI')
    setLang((settings.settingsLanguage as Lang) || 'zh')
  }

  useEffect(() => {
    applyTheme()
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    const changeHandler = () => {
      if (themeMode === 'system') applyTheme()
    }
    mq.addEventListener('change', changeHandler)
    return () => mq.removeEventListener('change', changeHandler)
  }, [themeMode])

  useEffect(() => {
    const handler = () => setMode(getMode())
    window.addEventListener('hashchange', handler)
    return () => window.removeEventListener('hashchange', handler)
  }, [])

  useEffect(() => {
    let cleanup: (() => void) | undefined
    api.onOpenSettings(() => {
      const currentHash = window.location.hash.replace('#', '').split('?')[0]
      if (currentHash !== '' && currentHash !== 'translator' && currentHash !== 'settings' && currentHash !== 'prompt-optimizer') return
      window.location.hash = '#settings'
      setMode('settings')
    }).then((unlisten) => {
      cleanup = unlisten
    })
    return () => {
      cleanup?.()
    }
  }, [])

  useEffect(() => {
    const resize = async () => {
      if (mode === 'settings') {
        await api.resizeWindow(SETTINGS_WINDOW_W, SETTINGS_WINDOW_H)
      } else if (mode === 'prompt-optimizer') {
        await api.resizeWindow(PROMPT_OPTIMIZER_WINDOW_W, PROMPT_OPTIMIZER_WINDOW_H)
      } else if (mode === '' || mode === 'translator') {
        await api.resizeWindow(TRANSLATOR_WINDOW_W, TRANSLATOR_WINDOW_H)
      }
    }
    resize()
  }, [mode])

  const closeSettings = async () => {
    try {
      await api.hideWindow()
    } catch (err) {
      console.error('[App] Error hiding window:', err)
    }
    window.location.hash = ''
    setMode('')
    await api.resizeWindow(TRANSLATOR_WINDOW_W, TRANSLATOR_WINDOW_H)
  }

  if (mode === 'lens') {
    return (
      <Suspense fallback={null}>
        <Lens />
      </Suspense>
    )
  }
  if (mode === 'settings') {
    return (
      <div className="h-screen w-screen overflow-hidden">
        <Suspense fallback={null}>
          <Settings onClose={closeSettings} onSettingsChange={applyTheme} />
        </Suspense>
      </div>
    )
  }
  if (mode === 'prompt-optimizer') {
    return <PromptOptimizer translateSource={translateSource} lang={lang} />
  }
  return <Translator translateSource={translateSource} lang={lang} />
}

export default App
