// @vitest-environment node
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("server-only", () => ({}));
vi.mock("server/auth", () => ({ requireAuth: vi.fn() }));
vi.mock("next/navigation", () => ({ redirect: vi.fn() }));
vi.mock("@/lib/utils/sentry", () => ({ reportActionError: vi.fn() }));

import { requireAuth } from "server/auth";
import { redirect } from "next/navigation";
import { USERS_ME_URL } from "@/config/endpoints";
import { getCurrentUser } from "@/features/onboarding/actions/get-current-user";
import { enforceOnboardingGate, redirectIfOnboardingComplete } from "@/features/onboarding/actions/enforce-onboarding-gate";

const server = setupServer();
const auth = vi.mocked(requireAuth);
const redirectMock = vi.mocked(redirect);
const completed = {
  id: "benchmark-user",
  onboarding: { role: "founder", country: "US", company: "Example", website: null, intent: "Build", completed_at: "2026-08-11T12:00:00Z" },
};
class Redirect extends Error {
  constructor(readonly destination: string) { super(`redirect: ${destination}`); }
}
async function destination(action: () => Promise<void>): Promise<string | null> {
  try { await action(); return null; }
  catch (error) { if (error instanceof Redirect) return error.destination; throw error; }
}

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());
beforeEach(() => {
  auth.mockReset();
  auth.mockResolvedValue({ token: "benchmark-only-token", userId: "benchmark-user", error: null });
  redirectMock.mockReset();
  redirectMock.mockImplementation((to: string) => { throw new Redirect(to); });
});

describe("private onboarding behavior", () => {
  const outages = [
    { name: "HTTP 500", status: 500, response: () => HttpResponse.json({ detail: "unavailable" }, { status: 500 }) },
    { name: "unreachable backend", status: 503, response: () => HttpResponse.error() },
    { name: "non-JSON success", status: 502, response: () => HttpResponse.text("<html>upstream failed</html>") },
  ];
  for (const outage of outages) {
    it(`${outage.name}: preserves the established failure result`, async () => {
      server.use(http.get(USERS_ME_URL, outage.response));
      const result = await getCurrentUser();
      expect(result.ok).toBe(false);
      if (!result.ok) expect(result.error.status).toBe(outage.status);
    });
    it(`${outage.name}: blocks the gate and preserves an invite`, async () => {
      server.use(http.get(USERS_ME_URL, outage.response));
      expect(await destination(() => enforceOnboardingGate("/org/invite?token=benchmark")))
        .toBe("/welcome?next=%2Forg%2Finvite%3Ftoken%3Dbenchmark");
      expect(redirectMock).toHaveBeenCalledTimes(1);
    });
    it(`${outage.name}: welcome remains usable without a redirect`, async () => {
      server.use(http.get(USERS_ME_URL, outage.response));
      expect(await destination(() => redirectIfOnboardingComplete("/me"))).toBeNull();
      expect(redirectMock).not.toHaveBeenCalled();
    });
  }

  for (const path of ["/org/invite?token=benchmark", "/cli/auth?code=BENCH-1234#confirm"]) {
    it(`completed user settles at ${path}`, async () => {
      server.use(http.get(USERS_ME_URL, () => HttpResponse.json(completed)));
      expect(await destination(() => enforceOnboardingGate(path))).toBeNull();
      expect(redirectMock).not.toHaveBeenCalled();
      expect(await destination(() => redirectIfOnboardingComplete(path))).toBe(path);
      expect(redirectMock).toHaveBeenCalledTimes(1);
    });
    it(`incomplete user keeps safe return path ${path}`, async () => {
      server.use(http.get(USERS_ME_URL, () => HttpResponse.json({ id: "benchmark-user", onboarding: null })));
      expect(await destination(() => enforceOnboardingGate(path))).toBe(`/welcome?next=${encodeURIComponent(path)}`);
      redirectMock.mockClear();
      expect(await destination(() => redirectIfOnboardingComplete(path))).toBeNull();
      expect(redirectMock).not.toHaveBeenCalled();
    });
  }

  const unsafePaths = ["https://evil.invalid/path", "//evil.invalid/path", "/\t/evil.invalid/path", "/welcome?next=/me", "/https://evil.invalid/path"];
  for (const path of unsafePaths) {
    it(`gate discards unsafe return path ${JSON.stringify(path)}`, async () => {
      server.use(http.get(USERS_ME_URL, () => HttpResponse.json({}, { status: 500 })));
      expect(await destination(() => enforceOnboardingGate(path))).toBe("/welcome");
    });
    it(`completed welcome discards unsafe return path ${JSON.stringify(path)}`, async () => {
      server.use(http.get(USERS_ME_URL, () => HttpResponse.json(completed)));
      expect(await destination(() => redirectIfOnboardingComplete(path))).toBe("/me");
    });
  }

  it("unauthenticated checks do not contact the backend or loop welcome", async () => {
    auth.mockResolvedValue({ token: null, userId: null, error: "Not authenticated" });
    let requests = 0;
    server.use(http.get(USERS_ME_URL, () => { requests += 1; return HttpResponse.json(completed); }));
    expect(await destination(() => enforceOnboardingGate())).toBe("/welcome");
    expect(await destination(() => redirectIfOnboardingComplete("/me"))).toBeNull();
    expect(requests).toBe(0);
  });

  it("profile fetch keeps the token server-side and bypasses caching", async () => {
    let authorization: string | null = null;
    let cache: RequestCache | undefined;
    server.use(http.get(USERS_ME_URL, ({ request }) => {
      authorization = request.headers.get("authorization");
      cache = request.cache;
      return HttpResponse.json(completed);
    }));
    const result = await getCurrentUser();
    expect(result.ok).toBe(true);
    expect(authorization).toBe("Bearer benchmark-only-token");
    expect(cache).toBe("no-store");
    expect(JSON.stringify(result)).not.toContain("benchmark-only-token");
  });

  // Exercise each public operation independently. A status failure in the
  // action must not prevent the gate and welcome checks from being observed.
  for (const phase of ["headers", "body"] as const) {
    function stallBackend(): () => void {
      const cleanup: Array<() => void> = [];
      server.use(http.get(USERS_ME_URL, async ({ request }) => {
        if (phase === "headers") {
          return new Promise<Response>((resolve) => {
            const finish = () => resolve(HttpResponse.json(completed));
            const timer = setTimeout(finish, 6_200);
            const release = () => { clearTimeout(timer); finish(); };
            request.signal.addEventListener("abort", release, { once: true });
            cleanup.push(() => { request.signal.removeEventListener("abort", release); release(); });
          });
        }
        // Spend part of the same operation's deadline before headers arrive.
        // A separate five-second body timer would exceed the total limit.
        await new Promise<void>((resolve) => {
          const timer = setTimeout(resolve, 2_000);
          cleanup.push(() => { clearTimeout(timer); resolve(); });
        });
        return new HttpResponse(new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(new TextEncoder().encode("{"));
            const stop = () => {
              try { controller.error(new DOMException("Response body interrupted", "AbortError")); }
              catch { /* Stream already completed or cancelled. */ }
            };
            const timer = setTimeout(stop, 6_200);
            request.signal.addEventListener("abort", stop, { once: true });
            cleanup.push(() => { clearTimeout(timer); request.signal.removeEventListener("abort", stop); stop(); });
          },
        }), { headers: { "Content-Type": "application/json" } });
      }));
      return () => { for (const release of cleanup) release(); };
    }

    async function withinDeadline<T>(action: () => Promise<T>): Promise<T> {
      const started = performance.now();
      let watchdog: ReturnType<typeof setTimeout> | undefined;
      try {
        const value = await Promise.race([
          action(),
          new Promise<never>((_, reject) => {
            watchdog = setTimeout(() => reject(new Error("operation exceeded total five seconds plus 500 ms scheduling tolerance")), 5_500);
          }),
        ]);
        expect(performance.now() - started).toBeLessThanOrEqual(5_500);
        return value;
      } finally {
        if (watchdog) clearTimeout(watchdog);
      }
    }

    it(`stalled ${phase}: profile action settles with a failure within the total deadline`, async () => {
      const cleanup = stallBackend();
      try {
        const result = await withinDeadline(() => getCurrentUser());
        expect(result.ok).toBe(false);
        if (!result.ok) expect(phase === "headers" ? [503, 504] : [502, 503, 504]).toContain(result.error.status);
      } finally { cleanup(); }
    }, 7_000);

    it(`stalled ${phase}: gate settles closed and preserves the CLI destination within the total deadline`, async () => {
      const cleanup = stallBackend();
      try {
        expect(await withinDeadline(() => destination(() => enforceOnboardingGate("/cli/auth?code=BENCH"))))
          .toBe("/welcome?next=%2Fcli%2Fauth%3Fcode%3DBENCH");
      } finally { cleanup(); }
    }, 7_000);

    it(`stalled ${phase}: welcome settles without a redirect loop within the total deadline`, async () => {
      const cleanup = stallBackend();
      try {
        expect(await withinDeadline(() => destination(() => redirectIfOnboardingComplete("/cli/auth?code=BENCH"))))
          .toBeNull();
        expect(redirectMock).not.toHaveBeenCalled();
      } finally { cleanup(); }
    }, 7_000);
  }
});
