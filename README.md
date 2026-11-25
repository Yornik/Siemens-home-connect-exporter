# Siemens Home Connect exporter

`hce` exports the state of Siemens and Bosch home appliances to Prometheus.

[![CI](https://github.com/Yornik/Siemens-home-connect-exporter/actions/workflows/ci.yml/badge.svg)](https://github.com/Yornik/Siemens-home-connect-exporter/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Yornik/Siemens-home-connect-exporter?sort=semver&display_name=tag)](https://github.com/Yornik/Siemens-home-connect-exporter/releases)
[![License: MIT](https://img.shields.io/github/license/Yornik/Siemens-home-connect-exporter)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.100-orange?logo=rust)](https://www.rust-lang.org/)
[![Image](https://img.shields.io/badge/image-ghcr.io%2Fyornik%2Fsiemens--home--connect--exporter-2496ED?logo=docker)](https://github.com/Yornik/Siemens-home-connect-exporter/pkgs/container/siemens-home-connect-exporter)
[![cosign signed](https://img.shields.io/badge/cosign-signed-blueviolet?logo=linuxfoundation)](docs/verifying-releases.md)
[![Renovate](https://img.shields.io/badge/renovate-enabled-1A8FEE?logo=renovatebot)](.github/renovate.json)

> [!WARNING]
> **The refresh token is a credential.** It gives software the read access
> of your Home Connect account: appliance names, state, and program
> details. It cannot start or stop programs, because the exporter requests
> read-only scopes. Keep the token in an encrypted secret. Do not put it
> in git, logs, or screenshots. If it leaks, remove the application's
> access in the Home Connect portal and run `hce authorize` again.

> [!IMPORTANT]
> **Unofficial project.** This is a personal, free-time project, built on
> the author's own title and in his own time. It is not made, endorsed,
> sponsored, or supported by BSH Hausgeräte GmbH, Robert Bosch GmbH, or
> Siemens AG. The author works for Siemens Digital Industries Software.
> Siemens household appliances come from BSH Hausgeräte GmbH. That company
> is separate from the author's employer. See
> [Disclaimer & trademarks](#disclaimer--trademarks).

`hce` keeps one Server-Sent Events stream open per appliance and serves the appliance state on `/metrics`.

## How it works

Home Connect appliances report to the BSH cloud. This exporter talks to the
Home Connect API with OAuth. It opens one event stream per appliance. The
stream is free: only the connection setup counts against the daily request
budget of 1000 calls. The exporter keeps 100 calls in reserve and never
polls while a stream is open.

## Metrics

- `homeconnect_appliance_info{ha_id,name,brand,type}` — every registered appliance.
- `homeconnect_connected{appliance}` — 1 while the appliance talks to the cloud.
- `homeconnect_operation_state{appliance,state}` — 1 for the active state, 0 for the rest.
- `homeconnect_door_open{appliance}` — 1 while the door is open.
- `homeconnect_program_progress_percent{appliance}` — program progress.
- `homeconnect_program_time_remaining_seconds{appliance}` — time left.
- `homeconnect_program_time_elapsed_seconds{appliance}` — time in program.
- `homeconnect_program_active{appliance,program}` — 1 for the running program.
- `homeconnect_program_events_total{appliance,event}` — counters, e.g. `ProgramFinished`.
- `homeconnect_up` — 1 after the appliance list call succeeded.
- `homeconnect_api_requests_total{call}` — API calls by type.
- `homeconnect_api_errors_total{call}` — failed API calls by type.

## Configure

All configuration is environment variables.

- `HCE_CLIENT_ID` — required. OAuth client id.
- `HCE_CLIENT_SECRET` — required. OAuth client secret.
- `HCE_REFRESH_TOKEN` — required. Refresh token from the authorize step.
- `HCE_LISTEN_ADDR` — scrape listener. Default `0.0.0.0:9780`.
- `HCE_API_BASE` — API root. Default is the production Home Connect API.
- `HCE_DAILY_REQUEST_BUDGET` — counted calls per day. Default `900`.

## Onboard

1. Create a developer account at `developer.home-connect.com`.
2. Register an application. Select the authorization code grant.
3. Set `HCE_CLIENT_ID` and `HCE_CLIENT_SECRET` from that application.
4. Run `hce authorize`. It prints one URL and waits.
5. Open the URL. Log in with your Home Connect account. Approve the access.
6. The command prints the refresh token. Set it as `HCE_REFRESH_TOKEN`.

The exporter requests the scopes `IdentifyAppliance` and `Monitor`. It
cannot start or stop programs.

## Run

```console
docker run -e HCE_CLIENT_ID=... -e HCE_CLIENT_SECRET=... -e HCE_REFRESH_TOKEN=... \
  -p 9780:9780 ghcr.io/yornik/siemens-home-connect-exporter:latest
```

Scrape the exporter:

```yaml
scrape_configs:
  - job_name: homeconnect
    static_configs:
      - targets: ["hce.home:9780"]
```

## Verify a release image

See [docs/verifying-releases.md](docs/verifying-releases.md).

## Disclaimer & trademarks

**This is an independent, unofficial project.** It is not made, endorsed,
sponsored, authorized, or supported by BSH Hausgeräte GmbH, Robert Bosch
GmbH, or Siemens AG. None of them has any affiliation with it.

**Personal work.** The author works for Siemens Digital Industries
Software. He built this project on his own title and in his own time. His
employer has no part in it. His employer did not authorize it, endorse
it, or support it. Siemens household appliances come from BSH Hausgeräte
GmbH. That company is separate from the author's employer.

**Trademarks.** Siemens, Bosch, and Home Connect are trademarks or
registered trademarks of their owners. This README uses them only to
identify the appliances and the service this software works with. That
use does not imply any affiliation or endorsement. This project does not
use any brand logo.

**No support.** BSH does not support this software. Do not contact BSH
customer service about it. Open an issue in this repository instead.

**Use at your own risk.** The exporter only reads data, and its access
token requests read-only scopes. Home Connect is a cloud service that
BSH can change, rate-limit, or block at any time. The software comes
without any warranty. See [LICENSE](LICENSE).
