#!/usr/bin/env python3
"""A minimal `yt-dlp`, standing in for the real one in tests.

It answers `--dump-single-json` the way yt-dlp answers for a music video, and
recognises five URLs of its own so the failure paths can be exercised without
YouTube (§14):

  …/blocked  exits non-zero with the sentence a refused lookup ends on
  …/garbage  prints something that is not JSON
  …/proxy    answers with the proxy environment it was started with as the
             title (or "direct" when there is none), so a test can see what the
             child inherited
  …/client   answers with the player client it was asked through as the title
  …/fallback refuses `tv_simply` the way YouTube refuses a client it does not
             like, and answers through any other — the shape of the real
             failure the client ladder exists for
"""

import json
import os
import sys

url = sys.argv[-1]


def player_client():
    """What `--extractor-args youtube:player_client=…` asked for, if anything."""
    args = sys.argv[1:]
    for flag, value in zip(args, args[1:]):
        if flag == "--extractor-args" and value.startswith("youtube:player_client="):
            return value.split("=", 1)[1]
    return "none"


def unavailable():
    sys.stderr.write(
        "WARNING: [youtube] player = %s\n" % player_client()
        + "ERROR: [youtube] fJ9rUzIMcZQ: This video is not available\n"
    )
    sys.exit(1)


if "client" in url:
    print(json.dumps({"title": player_client()}))
    sys.exit(0)

if "fallback" in url and player_client() == "tv_simply":
    unavailable()

if "blocked" in url:
    sys.stderr.write(
        "WARNING: [youtube] player = web\n"
        "ERROR: [youtube] fJ9rUzIMcZQ: Sign in to confirm you're not a bot\n"
    )
    sys.exit(1)

if "garbage" in url:
    print("this is not JSON")
    sys.exit(0)

if "proxy" in url:
    print(json.dumps({"title": os.environ.get("HTTPS_PROXY") or "direct"}))
    sys.exit(0)

print(
    json.dumps(
        {
            "title": "Queen - Bohemian Rhapsody (Official Video)",
            "artist": "Queen",
            "track": "Bohemian Rhapsody",
            "uploader": "Queen Official",
            "formats": [{"url": "https://example.invalid/nothing"}],
        }
    )
)
