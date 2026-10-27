# Security incident and vulnerability response

How DODI SMART Ltd. handles an actively exploited vulnerability or a severe
security incident in the Dira CLI (`dira` and `dirad`), including the reports the EU Cyber
Resilience Act (Regulation (EU) 2024/2847, Article 14) requires. Article 14 has
applied since **11 September 2026**, to products already on the market as well.

Keep this file short enough to follow under pressure. Product-specific details
live in the CRA architecture document: https://github.com/dodi-smart/dirahq-cloud/blob/develop/docs/cra/dira-cra.html.

## Roles

Fill these in before you need them. One person may hold more than one role,
but every role needs a named deputy.

| Role | Responsibility | Person | Deputy |
|---|---|---|---|
| Incident lead | Declares the incident, runs the response, decides on containment | Asen Lekov | Asen Lekov |
| Reporter of record | Decides whether it is reportable and submits the CRA reports | Asen Lekov | Asen Lekov |
| Communications | Tells affected customers, partners and users | Asen Lekov | Asen Lekov |

## Contacts

| Who | How |
|---|---|
| Security mailbox (incoming reports) | asen.lekov@dodi.li |
| ENISA Single Reporting Platform | Not registered yet |
| CSIRT designated as coordinator | CERT Bulgaria, Bulgaria's national CSIRT, https://www.govcert.bg (the Member State of the manufacturer's main establishment) |
| Hosting providers' security support | GitHub (source, CI, release binaries): https://hackerone.com/github; Vercel (serves the installers at dirahq.sh): https://hackerone.com/vercel or responsible.disclosure@vercel.com |

## Is it reportable?

Two kinds of event start a reporting clock. The clock starts when **anyone in
the organisation becomes aware** of the event, not when the investigation ends.

- **Actively exploited vulnerability:** there is reliable evidence that someone
  has exploited a vulnerability in the product without permission.
- **Severe incident:** an incident that harms, or can harm, the availability,
  authenticity, integrity or confidentiality of the product's data or functions,
  or that led or can lead to malicious code being run in the product or on a
  user's network.

When in doubt, treat it as reportable. An early warning that turns out to be
unnecessary costs much less than a missed deadline.

## Timeline

| By | Step | Owner |
|---|---|---|
| T+0 | Open a private `sec:` issue, record the time of awareness, name the incident lead | Whoever noticed it |
| T+4h | Contain: use the cheapest control that removes the exposure (see below) | Incident lead |
| **T+24h** | **Early warning** to the CSIRT coordinator through the ENISA platform | Reporter of record |
| **T+72h** | **Notification** with an initial assessment | Reporter of record |
| Fix available | Ship the fix through the emergency path in the CRA document; publish the advisory | Incident lead |
| **Fix + 14 days** | **Final report** for an exploited vulnerability | Reporter of record |
| **Notification + 1 month** | **Final report** for a severe incident | Reporter of record |
| Without delay | Tell affected users what happened and what they can do | Communications |

## What each report contains

**Early warning (24 hours)**
- which product is affected and since when, as far as known
- whether the event is suspected to be unlawful or malicious
- the Member States where the product is made available, if relevant

**Notification (72 hours)**
- the product and versions affected
- the general nature of the exploit or incident and of the vulnerability
- corrective or mitigating measures taken, and those users can take
- how sensitive the information in the report is

**Final report**
- a description of the vulnerability or incident, its severity and impact
- information about the malicious actor, where available
- details of the security update or other measures that fixed it

## Containment controls

List the controls this product has, cheapest first, so nobody has to find them
during an incident.

1. **Withdraw a bad release.** Mark it as a prerelease or delete it on GitHub.
   `install.sh`, `install.ps1` and `dira update` on the stable channel resolve
   `releases/latest`, so they stop offering it.
2. **Roll users back.** `dira update --version <last good>` downgrades in place.
3. **Roll back the installers.** They are served from the `dirahq-landing` Vercel
   project: use its instant rollback, then run that repo's Installer Drift
   workflow to confirm the served scripts match `main` here.
4. **Stop a device's syncs being accepted** in Dira cloud: revoke its runner
   token there. Revocation fails closed, and the device gets `403 runner_revoked`.

## Evidence

Before changing anything, keep copies of the logs, requests, files and database
rows involved, with their timestamps. Provider log retention is short: export
what you need on the first day.

## Afterwards

Within two weeks of closing, hold a review: timeline, root cause, what made
detection or response slow, and the changes that follow. Every change becomes a
tracked issue. Update this runbook and the CRA document if either was wrong.
