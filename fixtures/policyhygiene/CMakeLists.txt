cmake_minimum_required(VERSION 3.20)
project(policyhygiene NONE)
# Deliberate OLD pin (deprecated download-timestamp behavior): flagged.
# CMP0135 is used because its OLD behavior still configures on every
# supported CMake (>= 3.24 introduction, not yet removed in 4.x).
cmake_policy(SET CMP0135 OLD)
# NEW pin: healthy, must not be flagged.
cmake_policy(SET CMP0077 NEW)
message(STATUS "policies pinned")
