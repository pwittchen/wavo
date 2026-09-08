#!/usr/bin/env python3
"""A minimal `yt-dlp`, standing in for the real one in tests.

It answers `--dump-single-json` the way yt-dlp answers for a music video, and
recognises three URLs of its own so the failure paths can be exercised without
YouTube (§14):

  …/blocked  exits non-zero with the sentence a refused lookup ends on
  …/garbage  prints something that is not JSON
  …/proxy    answers with the proxy environment it was started with as the
             title (or "direct" when there is none), so a test can see what the
             child inherited
"""

import json
import os
import sys

url = sys.argv[-1]

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
