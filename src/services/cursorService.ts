import { invoke } from '@tauri-apps/api/core';
import { CursorAccount } from '../types/cursor';

export interface CursorOAuthLoginStartResponse {
  loginId: string;
  verificationUri: string;
  expiresIn: number;
  intervalSeconds: number;
}

/** Cursor OAuth: 开始登录（生成 PKCE，返回浏览器 URL） */
export async function startCursorOAuthLogin(): Promise<CursorOAuthLoginStartResponse> {
  return await invoke('cursor_oauth_login_start');
}

/** Cursor OAuth: 等待轮询完成（用户在浏览器完成登录后返回账号） */
export async function completeCursorOAuthLogin(loginId: string): Promise<CursorAccount> {
  return await invoke('cursor_oauth_login_complete', { loginId });
}

/** Cursor OAuth: 取消登录 */
export async function cancelCursorOAuthLogin(loginId?: string): Promise<void> {
  return await invoke('cursor_oauth_login_cancel', { loginId: loginId ?? null });
}

export async function listCursorAccounts(): Promise<CursorAccount[]> {
  return await invoke('list_cursor_accounts');
}

export async function deleteCursorAccount(accountId: string): Promise<void> {
  return await invoke('delete_cursor_account', { accountId });
}

export async function deleteCursorAccounts(accountIds: string[]): Promise<void> {
  return await invoke('delete_cursor_accounts', { accountIds });
}

export async function importCursorFromJson(jsonContent: string): Promise<CursorAccount[]> {
  return await invoke('import_cursor_from_json', { jsonContent });
}

export async function importCursorFromLocal(): Promise<CursorAccount[]> {
  return await invoke('import_cursor_from_local');
}

export async function exportCursorAccounts(accountIds: string[]): Promise<string> {
  return await invoke('export_cursor_accounts', { accountIds });
}

export async function refreshCursorToken(accountId: string): Promise<CursorAccount> {
  return await invoke('refresh_cursor_token', { accountId });
}

export async function refreshAllCursorTokens(): Promise<number> {
  return await invoke('refresh_all_cursor_tokens');
}

export async function addCursorAccountWithToken(accessToken: string): Promise<CursorAccount> {
  return await invoke('add_cursor_account_with_token', { accessToken });
}

export async function updateCursorAccountTags(accountId: string, tags: string[]): Promise<CursorAccount> {
  return await invoke('update_cursor_account_tags', { accountId, tags });
}

export async function getCursorAccountsIndexPath(): Promise<string> {
  return await invoke('get_cursor_accounts_index_path');
}

export async function injectCursorAccount(accountId: string): Promise<string> {
  return await invoke('inject_cursor_account', { accountId });
}

export interface CursorSwitchLowMetric {
  label: string;
  left_percent: number;
}

export interface CursorSwitchHistoryItem {
  id: string;
  timestamp: number;
  reason: string;
  from_account_id?: string | null;
  from_email?: string | null;
  to_account_id: string;
  to_email: string;
  low_metrics?: CursorSwitchLowMetric[];
  threshold?: number | null;
  success: boolean;
  error?: string | null;
}

export async function listCursorSwitchHistory(): Promise<CursorSwitchHistoryItem[]> {
  return await invoke('list_cursor_switch_history');
}

export async function clearCursorSwitchHistory(): Promise<void> {
  await invoke('clear_cursor_switch_history');
}

export interface CursorModelUsage {
  model: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_cents: number;
  charged_cents?: number | null;
}

export interface CursorUsageBreakdown {
  start_ms: number;
  end_ms: number;
  total_cents: number;
  charged_cents?: number | null;
  grok_charged_cents?: number | null;
  events_complete: boolean;
  models: CursorModelUsage[];
}

export async function getCursorUsageBreakdown(accountId: string): Promise<CursorUsageBreakdown> {
  return await invoke('get_cursor_usage_breakdown', { accountId });
}

export interface CursorHardLimit {
  hard_limit_dollars: number | null;
  no_usage_based_allowed: boolean;
}

export async function getCursorHardLimit(accountId: string): Promise<CursorHardLimit> {
  return await invoke('get_cursor_hard_limit', { accountId });
}

export async function setCursorHardLimit(
  accountId: string,
  hardLimitDollars: number,
  noUsageBasedAllowed: boolean,
): Promise<CursorAccount> {
  return await invoke('set_cursor_hard_limit', { accountId, hardLimitDollars, noUsageBasedAllowed });
}
