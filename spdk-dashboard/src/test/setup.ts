// Shared test setup: jest-dom matchers + the MSW server lifecycle. Every
// test runs against intercepted fetch — a request no handler matches is an
// error, so a test can never silently hit a real backend.
import '@testing-library/jest-dom/vitest';
import { afterAll, afterEach, beforeAll } from 'vitest';
import { cleanup } from '@testing-library/react';
import { server } from './server';

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }));
afterEach(() => {
  server.resetHandlers();
  cleanup();
});
afterAll(() => server.close());
