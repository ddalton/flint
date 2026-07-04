// The session boundary: token lives in module memory, every request carries
// it, and any 401 anywhere drops the session and notifies the auth hook.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { http, HttpResponse } from 'msw';
import { server, TEST_TOKEN } from '../test/server';
import * as api from './client';

afterEach(() => {
  api.logout();
  api.setOnSessionExpired(null);
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
