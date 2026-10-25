// HTTP mechanics are shared; service mutations are never automatically retried.
export const MUTATION_TIMEOUT_MS = 180000;
const REQUEST_TIMEOUT_MS = 4500;

export function createDashboardApi(fetchImpl = globalThis.fetch) {
  async function request(path, options, timeoutMs, read) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), timeoutMs);
    try {
      const response = await fetchImpl(path, {
        method: "GET",
        ...options,
        headers: { Accept: "application/json", ...(options.headers ?? {}) },
        signal: controller.signal,
        cache: "no-store",
      });
      return await read(response);
    } finally {
      clearTimeout(timeout);
    }
  }

  function fetchJson(path, options = {}) {
    return request(path, options, REQUEST_TIMEOUT_MS, (response) => {
      if (!response.ok) throw new Error("HTTP " + response.status);
      return response.json();
    });
  }

  function sendServiceAction(serviceId, action, token, requestId) {
    return request("/dashboard/v1/services/" + encodeURIComponent(serviceId) + "/" + action, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Dashboard-Token": token },
      body: JSON.stringify({ requestId }),
    }, MUTATION_TIMEOUT_MS, async (response) => {
      const payload = await response.json().catch(() => null);
      return { response, payload };
    });
  }

  return { fetchJson, sendServiceAction };
}
