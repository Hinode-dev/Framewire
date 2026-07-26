<div align="center">

# Framewire

**A high-fps, low-latency one-way game screen sharing tool**

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D6.svg)](#requirements)
[![Status: WIP](https://img.shields.io/badge/status-personal%20MVP%20%2F%20work%20in%20progress-orange.svg)](#status--roadmap)

**[Download for Windows](https://hinode-dev.github.io/Framewire/)**

</div>

---

## What is this?

Framewire is a desktop app for streaming gameplay footage at **120fps+ with low latency to multiple friends at once**.

The streamer just launches a native Windows app; viewers just **open a URL in their browser** — no install, no login, no permission prompts.

## Why

- Discord's screen share caps out at **60fps**, even on paid plans — a real loss of information for competitive FPS players running 144Hz/240Hz setups
- Sunshine + Moonlight offer great quality/latency but are fundamentally **1-to-1 remote desktop** tools (the viewer gets input control, setup is heavy, multi-viewer isn't a supported scenario)
- Framewire targets the gap: **"Sunshine-grade quality, Discord-grade simplicity, for a small group."**

The primary use case: playing a competitive/co-op FPS while showing 4-5 friends on a voice call your gameplay at high fidelity, to discuss strategy in real time. No text/voice chat is implemented — it's meant to run alongside Discord, handling video only.

## Key Features

- 1440p/120fps or 1080p/144fps high-fps streaming
- Works **while the game is in exclusive fullscreen** (a hard requirement for competitive players who won't switch to borderless)
- Up to 5 simultaneous viewers (P2P mesh)
- Join via a 6-character room code; browser-only viewing
- Zero-copy GPU pipeline from capture to encode (NVENC with native D3D11 input)

## Requirements

| Role | Requirement |
|---|---|
| Streaming host | Windows 10 (1903+) / 11, NVIDIA GTX 1060 or newer (NVENC required), upload bandwidth ≈ 25 Mbps × number of viewers |
| Viewer | Latest Chrome / Edge with hardware decode enabled |
| Signaling server | A small Linux VPS |

## Status / Roadmap

This is a personal MVP and **not yet production-ready**.

- Feasibility spikes (capture/encode fps & latency measurements): done
- Full capture → encode → deliver pipeline: implemented
- P2P mesh multi-viewer streaming: verified working locally (same-machine test); real fps/latency numbers and 5-viewer host-bandwidth measurements on separate machines are still pending
- `DXGI_ERROR_ACCESS_LOST` under exclusive fullscreen: partially unresolved

## License

Dual-licensed under **MIT OR Apache-2.0**. See [`LICENSE-MIT`](LICENSE-MIT) / [`LICENSE-APACHE`](LICENSE-APACHE).
