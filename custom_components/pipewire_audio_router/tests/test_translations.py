"""`strings.json` is the source of truth for our user-facing text, but Home
Assistant never loads it for a *custom* integration — at runtime it reads
`translations/<language>.json` only (`helpers/translation.py` looks under
`<integration>/translations/`). So English text has to exist in both, and the two
drift silently: edit `strings.json` alone and the UI keeps showing the old string,
or the raw key.
"""

import json
from pathlib import Path

COMPONENT = Path(__file__).parent.parent


def test_english_translations_match_strings_json():
    strings = json.loads((COMPONENT / "strings.json").read_text())
    english = json.loads((COMPONENT / "translations" / "en.json").read_text())
    assert english == strings, "translations/en.json is out of date — copy strings.json over it"


def test_every_duck_scope_option_has_a_label():
    """The scope select's options are stored as `area` / `music_group`; the labels
    are the only place Home Assistant renders an explanation of a setting rather
    than its raw value, so a new scope without one shows up as a bare slug."""
    from custom_components.pipewire_audio_router.const import VOICE_DUCK_SCOPES

    labels = json.loads((COMPONENT / "translations" / "en.json").read_text())
    states = labels["entity"]["select"]["voice_duck_scope"]["state"]
    assert sorted(states) == sorted(VOICE_DUCK_SCOPES)
