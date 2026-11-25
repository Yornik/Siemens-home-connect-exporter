# Siemens Home Connect exporter

`hce` is a Prometheus exporter for Siemens and Bosch home appliances that
speak Home Connect. It keeps one Server-Sent Events stream open per
appliance and serves the appliance state on `/metrics`.

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
