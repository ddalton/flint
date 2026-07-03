// Central API client: holds the bearer token in memory (never localStorage —
// a page refresh re-authenticates) and stamps every request. A 401 from any
// endpoint clears the session and notifies the auth hook so the app returns
// to the login page.

import type { components } from './schema';

// "viewer" | "admin", from the backend's Role enum via the generated spec.
export type Role = components['schemas']['Role'];

interface Session {
  token: string;
  role: Role;
}

let session: Session | null = null;
let onSessionExpired: (() => void) | null = null;

export const getRole = (): Role | null => session?.role ?? null;

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
  session = { token: body.token, role: body.role };
  return body.role;
};

export const logout = () => {
  session = null;
};

/** Drop-in replacement for fetch() that carries the bearer token. */
export const apiFetch = async (path: string, init: RequestInit = {}): Promise<Response> => {
  const headers = new Headers(init.headers);
  if (session) {
    headers.set('Authorization', `Bearer ${session.token}`);
  }
  const response = await fetch(path, { ...init, headers });
  if (response.status === 401) {
    session = null;
    onSessionExpired?.();
  }
  return response;
};
