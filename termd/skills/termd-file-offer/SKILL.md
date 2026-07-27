---
name: termd-file-offer
description: Offer completed local files to the user's paired termd clients. Use this skill after creating a report, archive, image, build artifact, or other file that the user should be able to download through termd.
---

# Offer a file through termd

Use this workflow only after the file is complete and ready for the user.

1. Confirm that the path resolves to a regular file. If the requested result is a directory, package it into an archive first and offer the archive.
2. Run:

   ```sh
   termd offer-file <PATH> [--socket <PATH>] [--json]
   ```

   Let the `termd` CLI resolve the daemon socket. Pass `--socket` only when an explicit custom socket path is already known. Use `--json` when structured output will help the surrounding workflow.
3. Show the command output to the user. A successful command means the daemon accepted the offer and queued one broadcast; it does not prove that a client or Push provider received it.
4. If the command fails, show the useful error details and do not claim that the file was offered. Correct straightforward path, file-type, readability, or socket-selection errors when the intended fix is clear, then retry only after the cause is addressed. Do not scan for daemon sockets.

If the daemon returns retryable `file_offer_delivery_busy`, wait before a bounded retry; do not loop continuously or claim success.

Each successful invocation creates one broadcast. Run `termd offer-file` again only when the user needs the file offered again.
