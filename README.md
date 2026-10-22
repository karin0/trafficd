# trafficd

trafficd records the per-user traffic of an [Xray](https://github.com/XTLS/Xray-core) server and a [Hysteria 2](https://v2.hysteria.network/) server into SQLite, and serves the statistics over HTTP.

Every `--interval` seconds, and before answering each request, it fetches the counters of both servers, resetting them unless `-R` is given, and appends a row to the `Record` table for each user and server that had traffic.

## Running

```sh
(cd sql && sh create.sh)   # creates sql/traffic.db with the schema in create.sql
trafficd <db path> <users.json> [-c config.json] [-b 127.0.0.1:10090 | -b <unix socket path>]
```

`users.json` maps each user name to an object with a numeric `id` and a `uuid`. A user matches the Xray stats of the emails whose part before `@` is the name, and the Hysteria user named by the part of the uuid before its first `-`.

`config.json` holds:

| key | value |
| --- | --- |
| `token` | the `Authorization` header every request must carry, unless `-A` is given |
| `tz` | an optional `[hours, minutes, seconds]` UTC offset that delimits months and years, the local offset by default |
| `xray.server` | the gRPC URL of the Xray API |
| `hysteria.url`, `hysteria.secret` | the Hysteria traffic stats API URL and its secret |

## API

- `POST /traffic` with a JSON body `{"year": 2026, "month": 10}` returns a protobuf `TrafficStats` from `protos/traffic_stats.proto` for that month. Omitting `month` selects the whole year, and an empty object the current month.
- `GET /traffic/<uid>` returns `{"sum": <bytes>, "until": <unix time>}`, the traffic of that user in the current month and the end of the month.

The `tid` of a row is the user id shifted left by 8 bits, with the low byte naming the server: 0 for Xray, 1 for Hysteria.

## Development

Building needs `protoc`, and the sqlx query macros check the queries against the database that `.env` names, so `sql/traffic.db` must exist. `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test` run in GitHub Actions on every push.
