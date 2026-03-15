# Python Room Agents Demo Plan

## Goal

Build a runnable Python demo that shows one director agent coordinating multiple
artist agents over a Skyffla room.

## Scope

- one `director` process in its own terminal
- multiple `artist` processes in their own terminals
- each agent uses the Python Skyffla wrapper for room chat
- each agent uses the OpenAI Python client for model-driven behavior
- `.env`-driven local configuration for API keys and model selection

## Initial Behavior

1. Director starts a room and waits for artists.
2. The director assigns numbers `1, 2, 3...` to artists in round-robin order.
3. For each turn, the director sends one compact task to one artist.
4. The artist uses OpenAI to draw that number as ASCII art inside a fixed grid.
5. The artist sends the art back to the director.
6. The director waits for that reply before assigning the next number.

## Constraints

- keep everything under `demos/python-room-agents`
- keep commands and paths repo-relative
- default to a fast model and make it configurable
- avoid requiring any repo-wide packaging changes to run the demo

## Follow-ups

- refine the task protocol between director and artists
- add richer style variation or director feedback per turn
- optionally move from chat-only routing to machine channels
