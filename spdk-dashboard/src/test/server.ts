// MSW request handlers — the dev/test stand-in for the warp backend
// (improvement-plan Decision 1: fixtures live at the network layer, never in
// the app bundle). Defaults model a healthy runj-like cluster; individual
// tests override with server.use(...).
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import type { components } from '../api/schema';
import { makeDashboardData, makeEventsResponse } from './fixtures';

type Schemas = components['schemas'];

export const TEST_TOKEN = 'test-bearer-token';

export const handlers = [
  http.post('/api/login', async ({ request }) => {
    const body = (await request.json()) as Schemas['LoginRequest'];
    if (body.password !== 'right-password') {
      return HttpResponse.json({ error: 'invalid credentials' }, { status: 401 });
    }
    const response: Schemas['LoginResponse'] = {
      token: TEST_TOKEN,
      role: body.username === 'viewer' ? 'viewer' : 'admin',
      expires_in_secs: 28800,
    };
    return HttpResponse.json(response);
  }),

  http.get('/api/dashboard', ({ request }) => {
    if (request.headers.get('authorization') !== `Bearer ${TEST_TOKEN}`) {
      return HttpResponse.json({ error: 'unauthorized' }, { status: 401 });
    }
    return HttpResponse.json(makeDashboardData());
  }),

  http.get('/api/events', ({ request }) => {
    if (request.headers.get('authorization') !== `Bearer ${TEST_TOKEN}`) {
      return HttpResponse.json({ error: 'unauthorized' }, { status: 401 });
    }
    return HttpResponse.json(makeEventsResponse());
  }),
];

export const server = setupServer(...handlers);
