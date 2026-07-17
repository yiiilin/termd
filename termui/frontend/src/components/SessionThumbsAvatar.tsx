import { Avatar, Style } from "@dicebear/core";
import thumbs from "@dicebear/styles/thumbs.json";
import { useMemo } from "react";
import type { UUID } from "../protocol/types";

const thumbsStyle = new Style(thumbs);
const SESSION_AVATAR_CACHE_LIMIT = 128;
const avatarSources = new Map<UUID, string>();

function sessionThumbsDataUri(sessionId: UUID): string {
  const cached = avatarSources.get(sessionId);
  if (cached) {
    avatarSources.delete(sessionId);
    avatarSources.set(sessionId, cached);
    return cached;
  }
  const source = new Avatar(thumbsStyle, {
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

export function SessionThumbsAvatar(props: { sessionId: UUID; className?: string }) {
  const source = useMemo(() => sessionThumbsDataUri(props.sessionId), [props.sessionId]);
  return (
    <img
      className={props.className}
      data-avatar-style="thumbs"
      data-session-avatar={props.sessionId}
      src={source}
      alt=""
      draggable={false}
    />
  );
}
