const SECRET_QUERY_PARAM_PATTERNS = [
  /token/i,
  /secret/i,
  /private/i,
  /signature/i,
  /ciphertext/i,
  /authorization/i,
  /bearer/i,
];

export function stripSensitiveUrlParts(rawUrl: string): string {
  const cleanUrl = rawUrl.trim();
  try {
    const parsed = new URL(cleanUrl);
    // 中文注释：relay_token 等 admission secret 允许作为临时连接材料，但不能进入 IndexedDB。
    for (const key of [...parsed.searchParams.keys()]) {
      if (isSensitiveQueryParam(key)) {
        parsed.searchParams.delete(key);
      }
    }
    parsed.hash = "";
    return parsed.toString();
  } catch {
    return stripFragmentForInvalidUrl(cleanUrl);
  }
}

export function displayUrlWithoutQueryOrFragment(rawUrl: string): string {
  try {
    const parsed = new URL(rawUrl);
    parsed.search = "";
    parsed.hash = "";
    return parsed.toString();
  } catch {
    return stripQueryAndFragmentForInvalidUrl(rawUrl);
  }
}

function isSensitiveQueryParam(key: string): boolean {
  return SECRET_QUERY_PARAM_PATTERNS.some((pattern) => pattern.test(key));
}

function stripFragmentForInvalidUrl(rawUrl: string): string {
  return rawUrl.split("#")[0] ?? rawUrl;
}

function stripQueryAndFragmentForInvalidUrl(rawUrl: string): string {
  return (rawUrl.split("?")[0] ?? rawUrl).split("#")[0] ?? rawUrl;
}
