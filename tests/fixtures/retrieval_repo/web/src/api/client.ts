import { SessionStore, refreshSessionToken } from "../session/refreshToken";

export class ApiClient {
  constructor(
    private readonly store: SessionStore,
    private readonly fetcher: typeof fetch,
  ) {}

  async get(path: string): Promise<unknown> {
    let session = this.store.read();
    if (session && session.expiresAt <= Date.now()) {
      session = await refreshSessionToken(this.store, this.fetcher);
    }
    const response = await this.fetcher(path, {
      headers: { authorization: `Bearer ${session?.accessToken ?? ""}` },
    });
    return response.json();
  }
}
