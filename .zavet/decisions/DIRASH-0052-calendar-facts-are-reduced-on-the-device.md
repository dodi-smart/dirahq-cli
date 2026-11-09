---
id: DIRASH-0052
title: Calendar facts are reduced on the device; attribution resolves locally; no titles, attendees or domains cross
status: proposed
guards:
  - cli/meetings/src/eligible.rs
  - cli/dirad/src/meetings/**
origin: session
verified: false
---

## Decision

The calendar connects **on the device**: a secret ICS URL, or OAuth with PKCE to Google
(`calendar.events.readonly`) or Microsoft (`Calendars.ReadBasic`), with tokens kept in the OS
keychain.

Each event is reduced on arrival and the raw response is dropped. What is kept:

- start and end;
- eligibility, and whether the meeting is remote or in person;
- a provider class;
- an attendee-count bucket;
- a recurring flag;
- organizer and attendee domains, **used only locally**.

What crosses the wire:

- `attendeeBucket` and `recurring`;
- `seriesKey`, which is HMAC(workspace salt, iCalUID);
- `external`, a boolean;
- attribution resolved **on the device** against a workspace directory fetched from the cloud
  (`client_ref`/`project_ref`).

Titles never ride this wire (see DIRASH-0053). A title hash is not used.

## Why

Calendar titles and attendee lists are content, and attendee domains identify third parties. Doing
the reduction and the domain → client match on the device means the cloud never holds data it
would have to protect, delete or justify under GDPR.

A title hash can be reversed by dictionary ("Standup"). A keyed hash of a random UID cannot, yet it
still groups a recurring series.

## Rejected

- **Cloud-side Google or Graph OAuth.** The cloud would process full event data. It is kept only
  for a future, admin-granted attendance reconcile.
- **Sending hashed attendee domains for the cloud to match.** The cloud holds the key, so a domain
  dictionary reverses them.
- **`freeBusy`.** It has no conference data and no attendee count.
- **macOS EventKit for now.** It needs a bundled `.app` with a usage string, and `dirad` is a bare
  binary.

## Agent directives

- A calendar response is converted to local facts and dropped in the same function. No title,
  attendee, email, domain or URL reaches a type that is stored beyond `calendar_cache` (which holds
  domains only, pruned after 2 days), logged or sent.
- Device bounds always win over scheduled bounds. Calendar-only time is clipped to observed
  presence.
- In-person events are never auto-recorded.
