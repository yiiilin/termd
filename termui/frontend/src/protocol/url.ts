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
    // 历史 URL 可能带有旧 transport 凭据；只保留可公开持久化的地址部分。
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
