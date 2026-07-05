// The session boundary: token lives in sessionStorage (per-tab; restored
// after a page refresh), every request carries it, and any 401 anywhere
// drops the session and notifies the auth hook.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { http, HttpResponse } from 'msw';
import { server, TEST_TOKEN } from '../test/server';
import * as api from './client';

const STORAGE_KEY = 'flint-dashboard-session';

afterEach(() => {
  api.logout();
  api.setOnSessionExpired(null);
  sessionStorage.clear();
});

describe('login', () => {
  it('stores the granted role and returns it', async () => {
    const role = await api.login('viewer', 'right-password');
    expect(role).toBe('viewer');
    expect(api.getRole()).toBe('viewer');
  });

  it('throws ApiError(401) with a human message on bad credentials', async () => {
    await expect(api.login('admin', 'wrong')).rejects.toMatchObject({
      status: 401,
      message: 'Invalid credentials',
    });
    expect(api.getRole()).toBeNull();
  });
});

describe('apiFetch', () => {
  it('stamps the bearer token on requests after login', async () => {
    await api.login('admin', 'right-password');
    let seen: string | null = null;
    server.use(
      http.get('/api/ping', ({ request }) => {
        seen = request.headers.get('authorization');
        return HttpResponse.json({});
      })
    );

    await api.apiFetch('/api/ping');
    expect(seen).toBe(`Bearer ${TEST_TOKEN}`);
  });

  it('clears the session and fires onSessionExpired on any 401', async () => {
    await api.login('admin', 'right-password');
    const expired = vi.fn();
    api.setOnSessionExpired(expired);
    server.use(http.get('/api/ping', () => HttpResponse.json({}, { status: 401 })));

    const response = await api.apiFetch('/api/ping');

    expect(response.status).toBe(401);
    expect(expired).toHaveBeenCalledTimes(1);
    expect(api.getRole()).toBeNull();
  });
});

describe('session persistence (refresh survival)', () => {
  it('writes the session to sessionStorage on login, clears it on logout', async () => {
    await api.login('admin', 'right-password');
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY)!)).toMatchObject({ role: 'admin' });

    api.logout();
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('clears the stored session on a 401 (stale token after backend restart)', async () => {
    await api.login('admin', 'right-password');
    server.use(http.get('/api/ping', () => HttpResponse.json({}, { status: 401 })));

    await api.apiFetch('/api/ping');
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('restores the session at module load — the page-refresh path', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ token: 'restored-token', role: 'viewer' }));
    vi.resetModules();
    const fresh = await import('./client');

    expect(fresh.hasSession()).toBe(true);
    expect(fresh.getRole()).toBe('viewer');

    let seen: string | null = null;
    server.use(
      http.get('/api/ping', ({ request }) => {
        seen = request.headers.get('authorization');
        return HttpResponse.json({});
      })
    );
    await fresh.apiFetch('/api/ping');
    expect(seen).toBe('Bearer restored-token');
  });

  it('boots without a session when storage holds garbage', async () => {
    sessionStorage.setItem(STORAGE_KEY, '{not json');
    vi.resetModules();
    const fresh = await import('./client');
    expect(fresh.hasSession()).toBe(false);
    expect(fresh.getRole()).toBeNull();
  });
});
