import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RefreshCw, Trash2, X } from 'lucide-react';
import { useEscClose } from '../hooks/useEscClose';
import { ModalErrorMessage } from './ModalErrorMessage';
import * as cursorService from '../services/cursorService';
import type {
  CursorHardLimit,
  CursorSwitchHistoryItem,
  CursorUsageBreakdown,
} from '../services/cursorService';
import { formatCursorUsageDollars } from '../types/cursor';

const TOP_MODELS = 8;

function formatTokens(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '0';
  if (value >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(2)}B`;
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(2)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(Math.round(value));
}

function formatDateTime(ms: number, locale: string): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return '';
  return d.toLocaleDateString(locale, { year: 'numeric', month: '2-digit', day: '2-digit' }) +
    ' ' + d.toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit' });
}

// ─── Usage breakdown ────────────────────────────────────────────────────────

interface UsageBreakdownModalProps {
  accountId: string | null;
  accountLabel: string;
  locale: string;
  onClose: () => void;
}

export function CursorUsageBreakdownModal({ accountId, accountLabel, locale, onClose }: UsageBreakdownModalProps) {
  const { t } = useTranslation();
  const [data, setData] = useState<CursorUsageBreakdown | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const isOpen = accountId != null;
  useEscClose(isOpen, onClose);

  useEffect(() => {
    if (!accountId) return;
    let cancelled = false;
    setLoading(true);
    setError('');
    setData(null);
    cursorService.getCursorUsageBreakdown(accountId)
      .then((result) => { if (!cancelled) setData(result); })
      .catch((e: unknown) => { if (!cancelled) setError(String(e)); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [accountId]);

  if (!isOpen) return null;

  const models = data?.models ?? [];
  const shown = models.slice(0, TOP_MODELS);
  const restCents = models.slice(TOP_MODELS).reduce((sum, item) => sum + item.total_cents, 0);
  const maxCents = shown.reduce((max, item) => Math.max(max, item.total_cents), 0);
  const hasCharged = data?.charged_cents != null;

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal" style={{ maxWidth: 640 }} onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>{t('cursor.usageBreakdown.title', '用量明细')} · {accountLabel}</h2>
          <button className="modal-close" onClick={onClose} aria-label={t('common.close', '关闭')}><X /></button>
        </div>
        <div className="modal-body">
          <ModalErrorMessage message={error} />
          {loading && (
            <div className="quota-empty"><RefreshCw size={14} className="loading-spinner" /> {t('common.loading', '加载中...')}</div>
          )}
          {!loading && data && (
            <>
              <div className="windsurf-credit-meta-row" style={{ justifyContent: 'space-between', marginBottom: 6 }}>
                <span className="windsurf-credit-used">
                  {t('cursor.usageBreakdown.period', '周期：{{start}} – {{end}}', {
                    start: formatDateTime(data.start_ms, locale),
                    end: formatDateTime(data.end_ms, locale),
                  })}
                </span>
                <span className="windsurf-credit-used">
                  {t('cursor.usageBreakdown.total', '合计 {{amount}}', { amount: formatCursorUsageDollars(data.total_cents) })}
                </span>
              </div>
              {hasCharged && (
                <div className="windsurf-credit-meta-row" style={{ justifyContent: 'space-between', marginBottom: 10 }}>
                  <span className="windsurf-credit-used" title={t('cursor.usageBreakdown.grokChargedHint', '按事件日志中模型名含 grok 或事件类型为 Grok Bot 的记录汇总，IDE 内调用的 Grok 模型也计入。')}>
                    {t('cursor.usageBreakdown.grokCharged', 'Grok 相关按需实付 {{amount}}', {
                      amount: formatCursorUsageDollars(data.grok_charged_cents ?? 0),
                    })}
                  </span>
                  <span className="windsurf-credit-used">
                    {t('cursor.usageBreakdown.charged', '按需实付合计 {{amount}}', {
                      amount: formatCursorUsageDollars(data.charged_cents ?? 0),
                    })}
                    {!data.events_complete && ` ${t('cursor.usageBreakdown.partial', '(部分)')}`}
                  </span>
                </div>
              )}
              {shown.length === 0 && (
                <div className="quota-empty">{t('cursor.usageBreakdown.empty', '本周期暂无用量记录')}</div>
              )}
              {shown.map((item) => {
                const ratio = maxCents > 0 ? (item.total_cents / maxCents) * 100 : 0;
                return (
                  <div key={item.model} className="quota-item windsurf-credit-item" style={{ marginBottom: 8 }}>
                    <div className="quota-header">
                      <span className="quota-label" title={item.model}>{item.model}</span>
                      <span className="quota-pct">
                        {formatCursorUsageDollars(item.total_cents)}
                        {item.charged_cents != null && item.charged_cents > 0 && (
                          <span className="windsurf-credit-used" style={{ marginLeft: 6, fontWeight: 400 }}>
                            {t('cursor.usageBreakdown.chargedInline', '实付 {{amount}}', {
                              amount: formatCursorUsageDollars(item.charged_cents),
                            })}
                          </span>
                        )}
                      </span>
                    </div>
                    <div className="windsurf-credit-meta-row">
                      <span className="windsurf-credit-used">
                        {t('cursor.usageBreakdown.tokens', '输入 {{input}} · 输出 {{output}} · 缓存读 {{cacheRead}}', {
                          input: formatTokens(item.input_tokens),
                          output: formatTokens(item.output_tokens),
                          cacheRead: formatTokens(item.cache_read_tokens),
                        })}
                      </span>
                    </div>
                    <div className="quota-bar-track">
                      <div className="quota-bar high" style={{ width: `${Math.min(100, Math.max(2, ratio))}%` }} />
                    </div>
                  </div>
                );
              })}
              {models.length > TOP_MODELS && (
                <div className="windsurf-credit-meta-row">
                  <span className="windsurf-credit-used">
                    {t('cursor.usageBreakdown.others', '其余 {{count}} 个模型合计 {{amount}}', {
                      count: models.length - TOP_MODELS,
                      amount: formatCursorUsageDollars(restCents),
                    })}
                  </span>
                </div>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

// ─── On-demand limit ────────────────────────────────────────────────────────

interface OnDemandModalProps {
  accountId: string | null;
  accountLabel: string;
  onClose: () => void;
  onSaved: () => void | Promise<void>;
}

export function CursorOnDemandModal({ accountId, accountLabel, onClose, onSaved }: OnDemandModalProps) {
  const { t } = useTranslation();
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [enabled, setEnabled] = useState(false);
  const [limitInput, setLimitInput] = useState('');
  const isOpen = accountId != null;
  useEscClose(isOpen && !saving, onClose);

  useEffect(() => {
    if (!accountId) return;
    let cancelled = false;
    setLoading(true);
    setError('');
    cursorService.getCursorHardLimit(accountId)
      .then((result: CursorHardLimit) => {
        if (cancelled) return;
        setEnabled(!result.no_usage_based_allowed);
        setLimitInput(result.hard_limit_dollars != null ? String(Math.round(result.hard_limit_dollars)) : '');
      })
      .catch((e: unknown) => { if (!cancelled) setError(String(e)); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [accountId]);

  if (!isOpen) return null;

  const parsedLimit = Number.parseInt(limitInput, 10);
  // Cursor 要求启用按需时上限为正整数；关闭时上限值不生效，传 0 即可。
  const limitInvalid = enabled && (!Number.isFinite(parsedLimit) || parsedLimit <= 0);

  const handleSave = async () => {
    if (!accountId || limitInvalid) return;
    setSaving(true);
    setError('');
    try {
      await cursorService.setCursorHardLimit(accountId, enabled ? parsedLimit : 0, !enabled);
      await onSaved();
      onClose();
    } catch (e: unknown) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="modal-overlay" onClick={() => !saving && onClose()}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>{t('cursor.onDemand.title', '按需使用设置')} · {accountLabel}</h2>
          <button className="modal-close" onClick={() => !saving && onClose()} aria-label={t('common.close', '关闭')}><X /></button>
        </div>
        <div className="modal-body">
          <ModalErrorMessage message={error} />
          {loading ? (
            <div className="quota-empty"><RefreshCw size={14} className="loading-spinner" /> {t('common.loading', '加载中...')}</div>
          ) : (
            <>
              <div className="settings-row">
                <div className="row-label">
                  <div className="row-title">{t('cursor.onDemand.enable', '启用按需使用')}</div>
                  <div className="row-desc">{t('cursor.onDemand.enableDesc', '套餐额度用完后按量计费继续使用；关闭则额度耗尽即停止。')}</div>
                </div>
                <div className="row-control">
                  <label className="switch">
                    <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} disabled={saving} />
                    <span className="slider"></span>
                  </label>
                </div>
              </div>
              {enabled && (
                <div className="settings-row">
                  <div className="row-label">
                    <div className="row-title">{t('cursor.onDemand.limit', '月度上限（美元）')}</div>
                    <div className="row-desc">{t('cursor.onDemand.limitDesc', '本周期按需花费达到该金额后停止；必须是正整数。')}</div>
                  </div>
                  <div className="row-control">
                    <div className="settings-inline-input">
                      <span className="settings-input-unit">$</span>
                      <input
                        type="number"
                        min={1}
                        step={1}
                        className="settings-select settings-select--input-mode settings-select--with-unit"
                        value={limitInput}
                        onChange={(e) => setLimitInput(e.target.value.replace(/[^0-9]/g, ''))}
                        disabled={saving}
                      />
                    </div>
                  </div>
                </div>
              )}
              <p className="row-desc" style={{ marginTop: 8 }}>
                {t('cursor.onDemand.notice', '此设置直接写入 Cursor 官网账号，与 Dashboard → Spending 页面一致，保存后立即生效。')}
              </p>
            </>
          )}
        </div>
        <div className="modal-footer">
          <button className="btn btn-secondary" onClick={onClose} disabled={saving}>{t('common.cancel')}</button>
          <button className="btn btn-primary" onClick={handleSave} disabled={saving || loading || limitInvalid}>
            {saving ? t('common.processing', '处理中...') : t('common.save', '保存')}
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── Switch history ─────────────────────────────────────────────────────────

interface SwitchHistoryModalProps {
  isOpen: boolean;
  locale: string;
  maskText: (value?: string | null) => string;
  onClose: () => void;
}

export function CursorSwitchHistoryModal({ isOpen, locale, maskText, onClose }: SwitchHistoryModalProps) {
  const { t } = useTranslation();
  const [items, setItems] = useState<CursorSwitchHistoryItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [error, setError] = useState('');
  useEscClose(isOpen && !clearing, onClose);

  useEffect(() => {
    if (!isOpen) return;
    let cancelled = false;
    setLoading(true);
    setError('');
    cursorService.listCursorSwitchHistory()
      .then((result) => { if (!cancelled) setItems(result); })
      .catch((e: unknown) => { if (!cancelled) setError(String(e)); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [isOpen]);

  if (!isOpen) return null;

  const reasonLabel = (reason: string) => {
    switch (reason) {
      case 'auto': return t('cursor.switchHistory.reasonAuto', '自动切号');
      case 'quota_alert': return t('cursor.switchHistory.reasonQuotaAlert', '预警快捷切换');
      case 'tray': return t('cursor.switchHistory.reasonTray', '托盘菜单');
      default: return t('cursor.switchHistory.reasonManual', '手动');
    }
  };

  const handleClear = async () => {
    setClearing(true);
    setError('');
    try {
      await cursorService.clearCursorSwitchHistory();
      setItems([]);
    } catch (e: unknown) {
      setError(String(e));
    } finally {
      setClearing(false);
    }
  };

  return (
    <div className="modal-overlay" onClick={() => !clearing && onClose()}>
      <div className="modal" style={{ maxWidth: 720 }} onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>{t('cursor.switchHistory.title', '切号历史')}</h2>
          <button className="modal-close" onClick={() => !clearing && onClose()} aria-label={t('common.close', '关闭')}><X /></button>
        </div>
        <div className="modal-body">
          <ModalErrorMessage message={error} />
          {loading && (
            <div className="quota-empty"><RefreshCw size={14} className="loading-spinner" /> {t('common.loading', '加载中...')}</div>
          )}
          {!loading && items.length === 0 && (
            <div className="quota-empty">{t('cursor.switchHistory.empty', '暂无切号记录')}</div>
          )}
          {!loading && items.length > 0 && (
            <div className="account-table-container">
              <table className="account-table">
                <thead>
                  <tr>
                    <th>{t('cursor.switchHistory.time', '时间')}</th>
                    <th>{t('cursor.switchHistory.reason', '原因')}</th>
                    <th>{t('cursor.switchHistory.from', '从')}</th>
                    <th>{t('cursor.switchHistory.to', '到')}</th>
                    <th>{t('cursor.switchHistory.result', '结果')}</th>
                  </tr>
                </thead>
                <tbody>
                  {items.map((item) => (
                    <tr key={item.id}>
                      <td>{formatDateTime(item.timestamp * 1000, locale)}</td>
                      <td>
                        {reasonLabel(item.reason)}
                        {item.low_metrics && item.low_metrics.length > 0 && (
                          <div className="kiro-table-subline">
                            {item.low_metrics.map((m) => `${m.label} ${m.left_percent}%`).join(' · ')}
                            {item.threshold != null ? ` (≤ ${item.threshold}%)` : ''}
                          </div>
                        )}
                      </td>
                      <td>{item.from_email ? maskText(item.from_email) : '—'}</td>
                      <td>{maskText(item.to_email || item.to_account_id)}</td>
                      <td>
                        {item.success
                          ? <span className="status-pill success">{t('cursor.switchHistory.success', '成功')}</span>
                          : <span className="status-pill warning" title={item.error ?? undefined}>{t('cursor.switchHistory.failed', '失败')}</span>}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
        <div className="modal-footer">
          <button className="btn btn-danger" onClick={handleClear} disabled={clearing || items.length === 0}>
            <Trash2 size={14} /> {clearing ? t('common.processing', '处理中...') : t('cursor.switchHistory.clear', '清空记录')}
          </button>
          <button className="btn btn-secondary" onClick={onClose} disabled={clearing}>{t('common.close', '关闭')}</button>
        </div>
      </div>
    </div>
  );
}
