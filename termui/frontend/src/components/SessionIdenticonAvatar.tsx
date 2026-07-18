import { Avatar, Style } from "@dicebear/core";
import identicon from "@dicebear/styles/identicon.json";
import { useMemo } from "react";
import type { UUID } from "../protocol/types";

const identiconStyle = new Style(identicon);
const SESSION_AVATAR_CACHE_LIMIT = 128;
const avatarSources = new Map<UUID, string>();

function sessionIdenticonDataUri(sessionId: UUID): string {
  const cached = avatarSources.get(sessionId);
  if (cached) {
    avatarSources.delete(sessionId);
    avatarSources.set(sessionId, cached);
    return cached;
  }
  const source = new Avatar(identiconStyle, {
    seed: sessionId,
    size: 64,
  }).toDataUri();
  avatarSources.set(sessionId, source);
  if (avatarSources.size > SESSION_AVATAR_CACHE_LIMIT) {
    const oldestSessionId = avatarSources.keys().next().value;
    if (oldestSessionId !== undefined) {
      avatarSources.delete(oldestSessionId);
    }
  }
  return source;
}

export function SessionIdenticonAvatar(props: { sessionId: UUID; className?: string }) {
  const source = useMemo(() => sessionIdenticonDataUri(props.sessionId), [props.sessionId]);
  return (
    <img
      className={props.className}
      data-avatar-style="identicon"
      data-session-avatar={props.sessionId}
      src={source}
      alt=""
      draggable={false}
    />
  );
}
