# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Developers who pay for Claude (Pro/Max), ChatGPT (Plus/Pro), Gemini, Antigravity, Grok, Kimi, Meta, Devin or Vertex AI and want to use those subscriptions from any tool: Claude Code, Codex, Cursor-style editors, OpenAI/Anthropic SDKs, scripts. They run CLIProxyAPI-Rust either on their own laptop or on a small VPS shared with a few teammates (confirmed: both matter equally). They glance at the dashboard between terminal sessions to answer three questions: is it running, which accounts are healthy, and where is my traffic going.

## Product Purpose

CLIProxyAPI-Rust is a single fast Rust binary that exposes OpenAI (Chat Completions + Responses, including websockets), Anthropic Messages and Gemini compatible endpoints, and serves them from a pool of OAuth accounts and API keys with round-robin routing, per-model cooldowns and automatic token refresh. Success: point any client at one base URL and forget about which account or provider answers.

## Positioning

A lean rewrite of CLIProxyAPI (Go): one binary, no runtime, an embedded dashboard, native Codex websocket pass-through, and cross-format translation through a single intermediate representation. Auth files stay compatible with CLIProxyAPI so users can switch without signing in again.

## Operating Context

- Clients: Claude Code (`ANTHROPIC_BASE_URL`), Codex (`config.toml` provider, websocket transport), OpenAI SDKs, Gemini clients.
- Accounts are added with `cliproxyapi-rust login claude|codex`, from the dashboard, or as API keys in `config.yaml`.
- OAuth redirects go to fixed localhost ports (54545 Claude, 1455 Codex, 51121 Antigravity; Devin uses any free port). On a remote server the user pastes the redirect URL back into the dashboard. Grok, Kimi and Meta use device codes, which need no redirect. Vertex takes a pasted service account key.
- Config hot-reloads; the dashboard's GUI settings editor updates the same `config.yaml`, retaining its layout, comments, and compatibility fields. Bind address, port, HTTPS, and debug logging changes need a restart.

## Capabilities and Constraints

- Providers: Claude (OAuth + key), Codex/ChatGPT (OAuth + key), Gemini (key), Vertex AI (service account + key), Antigravity (OAuth), Grok/xAI (device code + key), Kimi (device code + key), Meta (device code + key), Devin (OAuth), any OpenAI-compatible endpoint. Images through `/v1/images/*`, xAI video through `/v1/videos/*`.
- Dashboard is vanilla HTML/CSS/JS embedded in the binary; no build step, no external requests (no CDNs, no web fonts).
- Management API is localhost-only unless `management-key` is set.
- Usage statistics are in-memory (reset on restart).
- Optional Claude/Codex quota notifications use the existing usage observations and provider polls. Recovery requires fresh provider confirmation, not an elapsed reset timer. A bounded delivery journal persists in the auth directory; webhook credentials stay separate from YAML and are write-only in the dashboard. Managed credential writes require Unix file permissions; Windows supports externally provisioned credentials.

## Brand Commitments

- Name: CLIProxyAPI-Rust. The command, crate and file names use `cliproxyapi-rust`. Repo mark: rose (#e11d48) rounded square in `assets/icon.svg`.
- Brief from the user, binding: "clean black OLED UI, simplistic".
- Copy in English.

## Evidence on Hand

No testimonials, benchmarks or user counts exist. Do not invent performance numbers or adoption claims.

## Product Principles

1. One URL, every client: the endpoint and how to connect are never more than one glance away.
2. Account health is the product: cooldowns, expiry and errors are shown plainly, with the time until recovery.
3. Simple over complete: fewer screens, fewer controls, sensible defaults in config.
4. Works the same on a laptop and on a server.
