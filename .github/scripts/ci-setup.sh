#!/usr/bin/env bash
# Host and board CI setup (apt packages required to compile edgefirst-camera).
set -euo pipefail
sudo apt-get update
sudo apt-get install -y nasm
