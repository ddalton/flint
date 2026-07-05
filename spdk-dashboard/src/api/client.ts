// Central API client: holds the bearer token in sessionStorage (per-tab,
// gone when the tab closes — never localStorage) and stamps every request.
// A page refresh restores the session; the first 401 (expired/revoked
// token, backend restart) clears it and notifies the auth hook so the app
// returns to the login page.

import type { components } from './schema';

// "viewer" | "admin", from the backend's Role enum via the generated spec.
export type Role = components['schemas']['Role'];

interface Session {
  token: string;
  role: Role;
}

const STORAGE_KEY = 'flint-dashboard-session';

const loadSession = (): Session | null => {
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as Session) : null;
  } catch {
    return null;
  }
};

let session: Session | null = loadSession();
let onSessionExpired: (() => void) | null = null;

// Storage failures (private mode, quota) degrade to the old
// in-memory-only behavior rather than breaking login.
const storeSession = (next: Session | null) => {
  session = next;
  try {
    if (next) sessionStorage.setItem(STORAGE_KEY, JSON.stringify(next));
    else sessionStorage.removeItem(STORAGE_KEY);
  } catch {
    /* in-memory only */
  }
};

export const getRole = (): Role | null => session?.role ?? null;

/** A restored (or live) session exists — the app can boot past login. */
export const hasSession = (): boolean => session !== null;

export const setOnSessionExpired = (cb: (() => void) | null) => {
  onSessionExpired = cb;
};

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

export const login = async (username: string, password: string): Promise<Role> => {
  const response = await fetch('/api/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
  });
  if (!response.ok) {
    throw new ApiError(
      response.status,
      response.status === 401 ? 'Invalid credentials' : `Login failed (HTTP ${response.status})`
    );
  }
  const body: components['schemas']['LoginResponse'] = await response.json();
  storeSession({ token: body.token, role: body.role });
  return body.role;
};

export const logout = () => {
  storeSession(null);
};

/** Drop-in replacement for fetch() that carries the bearer token. */
export const apiFetch = async (path: string, init: RequestInit = {}): Promise<Response> => {
  const headers = new Headers(init.headers);
  if (session) {
    headers.set('Authorization', `Bearer ${session.token}`);
  }
  const response = await fetch(path, { ...init, headers });
  if (response.status === 401) {
    storeSession(null);
    onSessionExpired?.();
  }
  return response;
};
