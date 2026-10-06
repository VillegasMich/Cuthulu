# Cuthulu — Product Vision

> The eye that never sleeps.

Cuthulu is a self-hosted dashboard for watching and controlling the services
running on a single Linux machine. It starts with Docker containers and is
designed to grow to other kinds of services later.

## Problem

Several long-running Docker services run on this machine (for example
`auto-git-commit-tool`, `claude-session-starter`, `producer-tag-on-merge`).
Checking on them today means jumping between `docker ps`, `docker logs -f`,
and `docker restart` in a terminal. There is no single place to see what is
up, what is down, and what each service is saying.

## Goals

1. **One place for everything.** On first load, show every container Docker
   knows about. Running ones first, stopped ones after. No manual
   registration needed.
2. **Status at a glance.** State (running / stopped / restarting / paused /
   unhealthy), uptime, image, ports, health check result, restart count.
3. **Logs.** Live-tail a service's logs, with history, search/filter,
   stdout/stderr distinction and pause/resume of auto-scroll.
4. **Control.** Start, stop and restart a service from the UI.
5. **Scale to any number of services.** 3 containers or 300 — the UI stays
   usable (search, filter, grouping) and the backend does not poll each
   container individually.
6. **Runs as a Docker service itself.** Cuthulu ships as a container image
   and monitors the host through the Docker socket.
7. **Developer look and feel.** Dense, monospace, keyboard-friendly, light and
   dark themes. It should look like a tool built by a developer, not a
   generated landing page. See [DESIGN.md](DESIGN.md).

## Non-goals (for now)

- Not a replacement for Portainer: no image management, volume management,
  network editing, or container creation.
- Not a metrics/alerting stack (Prometheus/Grafana). Basic CPU/memory may come
  later, long-term time series will not.
- Not multi-user. One trusted operator on their own machine.
- Not multi-host (yet). One machine, one Docker daemon.

## Users

A single developer running personal tools and side projects as containers on
their own Linux workstation or home server.

## Core user stories

| # | As the operator I want to…                                     | Phase |
|---|----------------------------------------------------------------|-------|
| 1 | open the dashboard and see every container and its state       | MVP   |
| 2 | search/filter the list by name, image, state or compose project | MVP   |
| 3 | open a service and see its details (image, ports, mounts, env keys, health) | MVP |
| 4 | live-tail a service's logs and scroll back through history     | MVP   |
| 5 | start / stop / restart a service, with confirmation for stop   | MVP   |
| 6 | switch between light and dark theme (default: follow OS)       | MVP   |
| 7 | see state changes appear without refreshing the page           | MVP   |
| 8 | group services by Docker Compose project                       | Next  |
| 9 | see CPU / memory usage per service                             | Next  |
| 10| monitor non-Docker services (systemd units, plain processes)   | Later |
| 11| get a desktop notification when a service dies                 | Later |

## Success criteria for the MVP

- `docker compose up -d` (or a single `docker run`) brings Cuthulu up.
- Opening `http://localhost:8686` shows all current containers within a second.
- Stopping a container from the terminal is reflected in the UI within ~1s,
  without a page reload.
- Logs of a chatty container stream smoothly without freezing the browser.
- Cuthulu does not let you stop itself by accident.
