import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { save } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { ExternalLink, FileCode, RotateCcw, Tag, FolderInput } from 'lucide-react';
import { useCodexAccountStore } from '../../../stores/useCodexAccountStore';
import { useCodexPelicanStore } from '../../../stores/useCodexPelicanStore';
import { artifactPelican, previewPelican, retryPelican } from '../../../services/codexPelicanService';
import { assignAccountsToCodexGroup, createCodexGroup, getCodexAccountGroups, type CodexAccountGroup } from '../../../services/codexAccountGroupService';
import { pelicanCompletedCount, type CodexPelicanArtifact, type CodexPelicanBatch, type CodexPelicanItem } from '../../../types/codexPelican';
import { SingleSelectDropdown } from '../../SingleSelectDropdown';
import { ModalErrorMessage, useModalErrorState } from '../../ModalErrorMessage';
import { TagEditModal } from '../../TagEditModal';
import { pelicanError } from './pelicanUtils';
import { queuePelicanOrganization } from './pelicanState';
import { PelicanPlanBadge } from './PelicanAccountSummary';

export const PELICAN_GROUPS_CHANGED = 'codex-pelican-groups-changed';

function PelicanItem({ batch, item, mask }: { batch: CodexPelicanBatch; item: CodexPelicanItem; mask: (value: string) => string }) {
  const { t } = useTranslation();
  const accounts = useCodexAccountStore((state) => state.accounts);
  const account = accounts.find((entry) => entry.id === item.accountId);
  const availableTags = useMemo(() => Array.from(new Set(accounts.flatMap((entry) => entry.tags ?? []))).sort(), [accounts]);
  const [artifact, setArtifact] = useState<CodexPelicanArtifact | null>(null);
  const [source, setSource] = useState<'rawReply' | 'htmlSource' | null>(null);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const rowRef = useRef<HTMLElement>(null);
  const [organize, setOrganize] = useState(false);
  const [groups, setGroups] = useState<CodexAccountGroup[]>([]);
  const [groupId, setGroupId] = useState('');
  const [newGroup, setNewGroup] = useState('');
  const [tag, setTag] = useState('');
  const [tagEditorOpen, setTagEditorOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const error = useModalErrorState();
  const terminal = item.status !== 'running' && item.status !== 'queued';
  const retryable = item.status === 'failed' || item.status === 'cancelled' || item.status === 'interrupted';

  useEffect(() => { if (!item.hasHtml) setArtifact(null); }, [item.hasHtml]);

  useEffect(() => {
    if (!terminal || !item.hasHtml || artifact) return;
    let disposed = false;
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting)) return;
      observer.disconnect();
      void artifactPelican(batch.id, item.id).then((value) => { if (!disposed) setArtifact(value); })
        .catch((cause) => { if (!disposed) error.set(pelicanError(cause, t)); });
    }, { rootMargin: '100px' });
    if (rowRef.current) observer.observe(rowRef.current);
    return () => { disposed = true; observer.disconnect(); };
  }, [artifact, batch.id, item.hasHtml, item.id, terminal]);

  useEffect(() => {
    if (!detailsOpen || !terminal || artifact) return;
    void artifactPelican(batch.id, item.id).then(setArtifact)
      .catch((cause) => error.set(pelicanError(cause, t)));
  }, [artifact, batch.id, detailsOpen, item.id, terminal]);

  const run = async (operation: () => Promise<void>) => {
    setBusy(true); error.clear(); setSaved(false);
    try { await operation(); } catch (cause) { error.set(pelicanError(cause, t)); }
    finally { setBusy(false); }
  };
  const loadArtifact = async () => artifact ?? await artifactPelican(batch.id, item.id).then((value) => { setArtifact(value); return value; });
  const showSource = (kind: 'rawReply' | 'htmlSource') => void run(async () => {
    await loadArtifact(); setSource(kind); setDetailsOpen(true);
  });
  const retry = () => void run(async () => {
    const next = await retryPelican(batch.id, item.id);
    useCodexPelicanStore.getState().receive(next);
    setDetailsOpen(false);
  });

  return <article className="pelican-result" ref={rowRef}>
    <div className="pelican-result-account"><strong>{mask(item.accountEmail || item.accountId)}</strong>{account && <span className="pelican-account-summary pelican-result-plan"><PelicanPlanBadge account={account} /></span>}</div>
    <span className={`pelican-status pelican-status-${item.status}`}>{t(`pelican.${item.status}`)}</span>
    <div className="pelican-result-summary">
      {item.responseModel && <span>{t('pelican.model')}: {item.responseModel}</span>}
      {item.startedAt && <span>{new Date(item.startedAt).toLocaleString()}</span>}
      {item.startedAt && <span>{(((item.finishedAt ?? Date.now()) - item.startedAt) / 1000).toFixed(1)}s</span>}
    </div>
    {artifact?.html && <button type="button" className="pelican-thumbnail" onClick={() => void run(() => previewPelican(batch.id, item.id))} aria-label={t('pelican.preview')}>
      <iframe title={t('pelican.preview')} srcDoc={'<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; style-src \'unsafe-inline\'; img-src data:; font-src data:;">' + artifact.html} sandbox="" tabIndex={-1} loading="lazy" />
    </button>}
    {!artifact?.html && <span className="pelican-result-empty-preview">{terminal && !item.hasHtml ? t('pelican.noHtml') : '—'}</span>}
    <div className="pelican-actions pelican-result-actions">
      {retryable && <button className="btn btn-secondary" disabled={busy} onClick={retry}><RotateCcw size={14} />{t('common.windowsOperation.retry')}</button>}
      <button className="btn btn-secondary" disabled={busy || !account} onClick={() => { error.clear(); setTagEditorOpen(true); }}><Tag size={14} />{t('pelican.tag')}</button>
      <button className="btn btn-secondary" disabled={busy} onClick={() => { setSource('rawReply'); setDetailsOpen(true); }}>{t('common.detail')}</button>
    </div>
    {detailsOpen && <div className="pelican-detail-overlay">
      <section className="pelican-detail-dialog" role="dialog" aria-modal="true" aria-labelledby={`pelican-detail-${item.id}`} onKeyDown={(event) => {
        event.stopPropagation();
        if (event.key === 'Escape') { setSource(null); setDetailsOpen(false); error.clear(); }
        if (event.key === 'Tab') {
          const controls = event.currentTarget.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), textarea:not(:disabled), [tabindex="0"]');
          const first = controls[0]; const last = controls[controls.length - 1];
          if (first && event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
          if (last && !event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
        }
      }}>
        <header className="pelican-detail-header"><div><h3 id={`pelican-detail-${item.id}`}>{mask(item.accountEmail || item.accountId)}</h3>{account && <PelicanPlanBadge account={account} />}<span className={`pelican-status pelican-status-${item.status}`}>{t(`pelican.${item.status}`)}</span></div>
          <button className="btn btn-secondary" autoFocus onClick={() => { setSource(null); setDetailsOpen(false); error.clear(); }}>{t('common.close')}</button></header>
        <div className="pelican-detail-meta"><span>{t('pelican.model')}: {item.responseModel || batch.model}</span>{item.startedAt && <span>{new Date(item.startedAt).toLocaleString()}</span>}{item.startedAt && <span>{(((item.finishedAt ?? Date.now()) - item.startedAt) / 1000).toFixed(1)}s</span>}</div>
        {item.error && <div className="pelican-detail-error"><ModalErrorMessage message={pelicanError(item.error, t)} /><button className="btn btn-secondary" disabled={busy || !retryable} onClick={retry}><RotateCcw size={14} />{t('common.windowsOperation.retry')}</button></div>}
        <ModalErrorMessage message={error.message} scrollKey={error.scrollKey} />
        <div className="pelican-actions pelican-detail-actions">
          <button className="btn btn-secondary" disabled={busy || !item.hasHtml} onClick={() => void run(() => previewPelican(batch.id, item.id))}><ExternalLink size={14} />{t('pelican.preview')}</button>
          <button className={`btn ${source === 'rawReply' ? 'btn-primary' : 'btn-secondary'}`} disabled={busy || !terminal} onClick={() => showSource('rawReply')}>{t('pelican.rawReply')}</button>
          <button className={`btn ${source === 'htmlSource' ? 'btn-primary' : 'btn-secondary'}`} disabled={busy || !item.hasHtml} onClick={() => showSource('htmlSource')}><FileCode size={14} />{t('pelican.htmlSource')}</button>
          <button className="btn btn-secondary" disabled={busy || !item.hasHtml} onClick={() => void run(async () => {
            const result = await loadArtifact();
            if (!result?.html) return;
            const path = await save({ defaultPath: `pelican-${batch.id}-${item.id}.html`, filters: [{ name: 'HTML', extensions: ['html'] }] });
            if (path) await invoke('save_text_file', { path, content: result.html });
          })}>{t('pelican.export')}</button>
          <button className="btn btn-secondary" disabled={busy} onClick={() => void run(async () => {
            if (!account) { error.set(t('pelican.missingAccount')); return; }
            if (!organize) {
              const loaded = await getCodexAccountGroups(); setGroups(loaded);
              setGroupId(loaded.find((group) => group.accountIds.includes(item.accountId))?.id ?? '');
            }
            setOrganize(!organize);
          })}><Tag size={14} />{t('pelican.tag')} / {t('pelican.group')}</button>
        </div>
        <p className="pelican-muted">{t('pelican.previewHelp')}</p>
        <p className="pelican-muted">{t('pelican.exportWarning')}</p>
        <pre className="pelican-detail-output">{source === 'htmlSource' ? artifact?.html ?? t('common.loading') : artifact?.rawReply ?? item.replyPreview ?? t('pelican.noHtml')}</pre>
        {organize && <section className="pelican-organize">
      <p className="pelican-muted">{t('pelican.groupHelp')}</p>
      <div className="pelican-inline"><SingleSelectDropdown value={groupId} options={groups.map((group) => ({ value: group.id, label: group.name }))}
        ariaLabel={t('pelican.group')} placeholder={t('pelican.group')} disabled={busy} onChange={(value) => { setGroupId(value); error.clear(); setSaved(false); }} />
        <button className="btn btn-secondary" disabled={busy || !groupId || !account} onClick={() => void run(() => queuePelicanOrganization(async () => {
          if (!useCodexAccountStore.getState().accounts.some((entry) => entry.id === item.accountId)) { error.set(t('pelican.missingAccount')); return; }
          const result = await assignAccountsToCodexGroup(groupId, [item.accountId]);
          if (!result) { error.set(t('accounts.groups.error.notFound')); return; }
          window.dispatchEvent(new Event(PELICAN_GROUPS_CHANGED)); setSaved(true);
        }))}><FolderInput size={14} />{t('pelican.group')}</button></div>
      <div className="pelican-inline"><input value={newGroup} maxLength={80} aria-label={t('accounts.groups.newPlaceholder')} placeholder={t('accounts.groups.newPlaceholder')} disabled={busy}
        onChange={(event) => { setNewGroup(event.target.value); error.clear(); setSaved(false); }} />
        <button className="btn btn-secondary" disabled={busy || !newGroup.trim() || !account} onClick={() => void run(() => queuePelicanOrganization(async () => {
          if (!useCodexAccountStore.getState().accounts.some((entry) => entry.id === item.accountId)) { error.set(t('pelican.missingAccount')); return; }
          const existingGroups = await getCodexAccountGroups();
          if (existingGroups.some((group) => group.name.toLowerCase() === newGroup.trim().toLowerCase())) { error.set(t('accounts.groups.error.duplicate')); return; }
          const created = await createCodexGroup(newGroup.trim());
          await assignAccountsToCodexGroup(created.id, [item.accountId]);
          setGroups(await getCodexAccountGroups()); setGroupId(created.id); setNewGroup('');
          window.dispatchEvent(new Event(PELICAN_GROUPS_CHANGED)); setSaved(true);
        }))}>{t('accounts.groups.createAndAdd')}</button></div>
      <div className="pelican-inline"><input value={tag} maxLength={20} aria-label={t('pelican.tag')} placeholder={t('pelican.tagPlaceholder')} disabled={busy}
        onChange={(event) => { setTag(event.target.value); error.clear(); setSaved(false); }} />
        <button className="btn btn-secondary" disabled={busy || !tag.trim() || !account} onClick={() => void run(() => queuePelicanOrganization(async () => {
          const latest = useCodexAccountStore.getState().accounts.find((entry) => entry.id === item.accountId);
          if (!latest) { error.set(t('pelican.missingAccount')); return; }
          const tags = Array.from(new Set([...(latest.tags ?? []), tag.trim().toLowerCase()]));
          if (tags.length > 10) { error.set(t('accounts.tagModal.error.tooMany', { max: 10 })); return; }
          await useCodexAccountStore.getState().updateAccountTags(item.accountId, tags); setTag(''); setSaved(true);
        }))}>{t('pelican.tag')}</button></div>
      {account?.tags?.length ? <p className="pelican-muted">{account.tags.join(' · ')}</p> : null}
      {saved && <p role="status">{t('pelican.saved')}</p>}
        </section>}
        {busy && <p className="pelican-muted" role="status">{t('common.loading')}</p>}
      </section>
    </div>}
    {!detailsOpen && <ModalErrorMessage message={error.message} scrollKey={error.scrollKey} />}
    <TagEditModal isOpen={tagEditorOpen} resetKey={`${item.accountId}:${account?.tags?.join('\u0001') ?? ''}`}
      initialTags={account?.tags ?? []} availableTags={availableTags} onClose={() => setTagEditorOpen(false)}
      onSave={async (tags) => {
        const latest = useCodexAccountStore.getState().accounts.find((entry) => entry.id === item.accountId);
        if (!latest) throw new Error(t('pelican.missingAccount'));
        await queuePelicanOrganization(() => useCodexAccountStore.getState().updateAccountTags(item.accountId, tags));
      }} />
  </article>;
}

export function PelicanResults({ batch, mask }: { batch: CodexPelicanBatch; mask: (value: string) => string }) {
  const { t } = useTranslation();
  const [, tick] = useState(0);
  useEffect(() => {
    if (batch.status !== 'running') return;
    const timer = window.setInterval(() => tick((value) => value + 1), 1000);
    return () => clearInterval(timer);
  }, [batch.status]);
  return <>
    <section className="pelican-progress-card">
      <div className="pelican-progress-header"><div><span className="pelican-eyebrow">{t('pelican.current')}</span><strong>{t('pelican.progress', { done: pelicanCompletedCount(batch), total: batch.items.length })}</strong></div><span className={`pelican-status pelican-status-${batch.status}`}>{t(`pelican.${batch.status}`)}</span></div>
      <div className="pelican-progress-track"><div style={{ width: `${batch.items.length ? pelicanCompletedCount(batch) / batch.items.length * 100 : 0}%` }} /></div>
      <div className="pelican-progress-meta"><span>{batch.model}</span><span>{batch.effort}</span><span>{t('pelican.concurrency')}: {batch.concurrency}</span><span>{(((batch.finishedAt ?? Date.now()) - batch.createdAt) / 1000).toFixed(0)}s</span></div>
      <details className="pelican-prompt-details"><summary>{t('pelican.prompt')}</summary><pre className="pelican-reply">{batch.prompt}</pre><p className="pelican-muted">{t('pelican.deliveryHelp')}</p><pre className="pelican-reply">{batch.deliveryInstructions}</pre></details>
    </section>
    <ModalErrorMessage message={batch.error ? pelicanError(batch.error, t) : undefined} />
    <div className="pelican-results-heading"><strong>{t('pelican.accounts')}</strong><span>{batch.items.length}</span></div>
    <div className="pelican-results">{batch.items.map((item) => <PelicanItem key={`${batch.id}:${item.id}`} batch={batch} item={item} mask={mask} />)}</div>
  </>;
}
