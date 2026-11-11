"""The scenario step vocabulary shared by the runner and both adapters.

Pure parsing, validation and predicates: nothing here touches a client, so
`steps_test.py` covers it without a display. See DOCUMENTATION.md, *Steps*.
"""

from __future__ import annotations

import fnmatch
import json
import re
import urllib.parse
from dataclasses import dataclass

# --- keys -------------------------------------------------------------------

# Canonical modifier names, in the order they are pressed.
MODIFIERS = ("ctrl", "alt", "shift", "super")
MODIFIER_ALIASES = {"control": "ctrl", "meta": "super"}

# X keysym name -> (DOM key, DOM code, Windows virtual key code). Named keys
# produce no text, as the runner has always sent them.
NAMED_KEYS = {
    "Return": ("Enter", "Enter", 13),
    "Escape": ("Escape", "Escape", 27),
    "Tab": ("Tab", "Tab", 9),
    "BackSpace": ("Backspace", "Backspace", 8),
    "Delete": ("Delete", "Delete", 46),
    "Insert": ("Insert", "Insert", 45),
    "Home": ("Home", "Home", 36),
    "End": ("End", "End", 35),
    "Page_Up": ("PageUp", "PageUp", 33),
    "Page_Down": ("PageDown", "PageDown", 34),
    "Left": ("ArrowLeft", "ArrowLeft", 37),
    "Right": ("ArrowRight", "ArrowRight", 39),
    "Up": ("ArrowUp", "ArrowUp", 38),
    "Down": ("ArrowDown", "ArrowDown", 40),
    **{f"F{number}": (f"F{number}", f"F{number}", 111 + number) for number in range(1, 13)},
}

# X keysym name -> (character, DOM code, Windows virtual key code) for keys
# that type a character on a US layout.
CHARACTER_KEYS = {
    "space": (" ", "Space", 32),
    "slash": ("/", "Slash", 191),
    "backslash": ("\\", "Backslash", 220),
    "period": (".", "Period", 190),
    "comma": (",", "Comma", 188),
    "minus": ("-", "Minus", 189),
    "equal": ("=", "Equal", 187),
    "semicolon": (";", "Semicolon", 186),
    "apostrophe": ("'", "Quote", 222),
    "bracketleft": ("[", "BracketLeft", 219),
    "bracketright": ("]", "BracketRight", 221),
    "grave": ("`", "Backquote", 192),
    **{letter: (letter, f"Key{letter.upper()}", ord(letter.upper())) for letter in "abcdefghijklmnopqrstuvwxyz"},
    **{digit: (digit, f"Digit{digit}", ord(digit)) for digit in "0123456789"},
}

# DOM modifier keys: (key, code, Windows virtual key code, CDP modifier bit).
DOM_MODIFIERS = {
    "alt": ("Alt", "AltLeft", 18, 1),
    "ctrl": ("Control", "ControlLeft", 17, 2),
    "super": ("Meta", "MetaLeft", 91, 4),
    "shift": ("Shift", "ShiftLeft", 16, 8),
}


@dataclass(frozen=True)
class Chord:
    """A key with the modifiers held while it is pressed."""

    modifiers: tuple[str, ...]
    key: str

    def xdotool(self) -> str:
        """The chord in xdotool's `key` syntax."""
        return "+".join((*self.modifiers, self.key))


def parse_key(spec: str) -> Chord:
    """`"ctrl+shift+z"` -> Chord(("ctrl", "shift"), "z"). Keys use X keysym
    names; letters are lower case. Shifted characters other than letters
    depend on the layout, so they are typed with `type` instead."""
    if not isinstance(spec, str) or not spec or spec != spec.strip():
        raise ValueError(f"a key needs a keysym name: {spec!r}")
    *raw_modifiers, key = spec.split("+") if spec != "+" else ["+"]
    modifiers = []
    for raw in raw_modifiers:
        name = MODIFIER_ALIASES.get(raw.lower(), raw.lower())
        if name not in MODIFIERS:
            raise ValueError(f"unknown modifier {raw!r} in {spec!r}")
        if name in modifiers:
            raise ValueError(f"repeated modifier {raw!r} in {spec!r}")
        modifiers.append(name)
    if key not in NAMED_KEYS and key not in CHARACTER_KEYS:
        raise ValueError(f"unknown key {key!r} in {spec!r}")
    if "shift" in modifiers and key in CHARACTER_KEYS and not (CHARACTER_KEYS[key][0].isalpha() or key == "space"):
        raise ValueError(f"{spec!r} depends on the keyboard layout; type the character instead")
    return Chord(tuple(sorted(modifiers, key=MODIFIERS.index)), key)


def dom_key_events(chord: Chord) -> list[dict]:
    """The CDP `Input.dispatchKeyEvent` parameters a user's chord produces:
    each modifier pressed in order, the key, then the modifiers released."""
    events = []
    mask = 0
    for name in chord.modifiers:
        key, code, key_code, bit = DOM_MODIFIERS[name]
        mask |= bit
        events.append({"type": "rawKeyDown", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": mask})
    if chord.key in NAMED_KEYS:
        key, code, key_code = NAMED_KEYS[chord.key]
        events.append({"type": "rawKeyDown", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": mask})
    else:
        character, code, key_code = CHARACTER_KEYS[chord.key]
        key = character.upper() if "shift" in chord.modifiers else character
        down = {"key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": mask}
        # A command chord types nothing; a bare or shifted character does.
        if mask & ~DOM_MODIFIERS["shift"][3]:
            events.append({"type": "rawKeyDown", **down})
        else:
            events.append({"type": "keyDown", **down, "text": key, "unmodifiedText": character})
    events.append({**events[-1], "type": "keyUp"})
    events[-1].pop("text", None)
    events[-1].pop("unmodifiedText", None)
    for name in reversed(chord.modifiers):
        key, code, key_code, bit = DOM_MODIFIERS[name]
        mask &= ~bit
        events.append({"type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": key_code, "modifiers": mask})
    return events


# --- accessible names -------------------------------------------------------

# ARIA roles -> the AT-SPI role names AccessKit exposes for them.
ATSPI_ROLES = {
    "button": ("push button", "button", "toggle button"),
    "checkbox": ("check box",),
    "combobox": ("combo box",),
    "heading": ("heading",),
    "link": ("link",),
    "listitem": ("list item",),
    "menuitem": ("menu item", "check menu item", "radio menu item"),
    "option": ("list item", "option"),
    "radio": ("radio button",),
    "switch": ("switch", "toggle button"),
    "tab": ("page tab",),
    "textbox": ("entry", "text"),
    "treeitem": ("tree item",),
}


def named_control(spec: dict, kind: str) -> dict:
    """The `clickNamed` query for one client: a shared `{"name", "role"}` or
    a per-client override under `electron`/`native`."""
    query = spec[kind] if kind in spec else spec
    if not isinstance(query, dict) or not isinstance(query.get("name"), str) or not query["name"]:
        raise ValueError(f"clickNamed needs an accessible name for {kind}")
    role = query.get("role")
    if role is not None and kind == "native" and "roles" not in query:
        if role not in ATSPI_ROLES:
            raise ValueError(f"no AT-SPI mapping for role {role!r}; give a native override")
        return {"name": query["name"], "roles": list(ATSPI_ROLES[role])}
    return {key: value for key, value in query.items() if key in ("name", "role", "roles")}


# --- faults -----------------------------------------------------------------

FAULT_STAGES = ("before", "after")
FAULT_ACTIONS = ("fail", "drop", "hold")


def fault_rule(spec: dict, kind: str) -> dict:
    """A validated fault for one client. `when` says whether the server
    receives the request (`after`) or not (`before`); `action` fails it with
    an HTTP status, drops the connection, or holds it until `release`."""
    rule = {
        "id": spec.get("id"),
        "method": str(spec.get("method", "")).upper(),
        "path": spec["path"][kind] if isinstance(spec.get("path"), dict) else spec.get("path"),
        "when": spec.get("when"),
        "action": spec.get("action"),
        "status": spec.get("status", 500),
        "times": spec.get("times", 1),
    }
    if not isinstance(rule["id"], str) or not rule["id"]:
        raise ValueError("a fault needs an id")
    if not re.fullmatch(r"[A-Z]+", rule["method"]):
        raise ValueError(f"fault {rule['id']} needs an HTTP method")
    if not isinstance(rule["path"], str) or not rule["path"].startswith("/") or any(char in rule["path"] for char in "?[]"):
        raise ValueError(f"fault {rule['id']} needs a path pattern for {kind}, with * as its only wildcard")
    if rule["when"] not in FAULT_STAGES:
        raise ValueError(f"fault {rule['id']} needs when: before or after")
    if rule["action"] not in FAULT_ACTIONS:
        raise ValueError(f"fault {rule['id']} needs action: fail, drop or hold")
    if not isinstance(rule["status"], int) or not 400 <= rule["status"] <= 599:
        raise ValueError(f"fault {rule['id']} status must be an HTTP error")
    if not isinstance(rule["times"], int) or rule["times"] < 1:
        raise ValueError(f"fault {rule['id']} times must be positive")
    return rule


def fault_matches(rule: dict, method: str, url_or_path: str) -> bool:
    """Method and path glob; `*` crosses `/`. Query strings never match."""
    path = urllib.parse.urlsplit(url_or_path).path
    return method.upper() == rule["method"] and fnmatch.fnmatchcase(path, rule["path"])


# --- expectations -----------------------------------------------------------

PREDICATES = ("equals", "contains", "length", "exists")
GIT_READS = ("status", "diff", "log", "show", "rev-parse", "branch", "ls-files", "cat-file", "remote", "for-each-ref")
# Commands that also write when given other arguments.
GIT_LISTING_FLAGS = {
    "branch": {"--list", "-a", "-r", "-v", "-vv", "--show-current"},
    "remote": {"-v"},
}


def select(value, path: list):
    """Walks keys and list indexes; a missing step selects None. `"*"` maps
    the rest of the path over a list and keeps the values it finds, so
    `["data", "*", "id"]` is every item's ID; a further `"*"` in the rest
    adds its values to the same list, so `["data", "*", "parts", "*",
    "status"]` is every part's status across the items."""
    for index, step in enumerate(path):
        if step == "*":
            if not isinstance(value, list):
                return None
            rest = path[index + 1:]
            found = [select(item, rest) for item in value]
            if "*" in rest:
                return [value for item in found if item is not None for value in item]
            return [item for item in found if item is not None]
        if isinstance(value, dict):
            value = value.get(step)
        elif isinstance(value, list) and isinstance(step, int) and -len(value) <= step < len(value):
            value = value[step]
        else:
            return None
    return value


def subset(expected, actual) -> bool:
    """Whether `expected` is a structural subset of `actual`."""
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(key in actual and subset(value, actual[key]) for key, value in expected.items())
    return expected == actual


def holds(spec: dict, value) -> bool:
    """Applies `where`, then every predicate the expectation names."""
    if "where" in spec:
        if not isinstance(value, list):
            return False
        value = [item for item in value if subset(spec["where"], item)]
    if "equals" in spec and value != spec["equals"]:
        return False
    if "contains" in spec:
        wanted = spec["contains"]
        if isinstance(value, str):
            if not isinstance(wanted, str) or wanted not in value:
                return False
        elif isinstance(value, list):
            if not any(subset(wanted, item) for item in value):
                return False
        elif not subset(wanted, value):
            return False
    if "length" in spec and (not isinstance(value, (list, str, dict)) or len(value) != spec["length"]):
        return False
    if "exists" in spec and (value is not None) != spec["exists"]:
        return False
    return True


def bounded(value, limit: int = 4000):
    """Evidence of an observed value, cut to a bounded size."""
    text = json.dumps(value, ensure_ascii=False, default=str)
    return value if len(text) <= limit else text[:limit] + "..."


def substitute(template: str, places: dict[str, str]) -> str:
    """Replaces `{fixture}` and `{project}`; sessions resolve separately."""
    return re.sub(r"\{(fixture|project)\}", lambda match: places[match.group(1)], template)


SESSION_PLACEHOLDER = re.compile(r"\{session:([^{}]+)\}")


# --- validation -------------------------------------------------------------

STEP_KINDS = ("wait", "click", "clickTarget", "clickNamed", "hover", "key", "type", "paste", "settle", "capture",
              "restart", "expect", "fault", "release", "drop", "pick", "ime", "gate", "drag")
FIXTURES = ("git", "github")
# Fixture provider modes: `interactions` adds the shell and question models
# and asks permission for the shell fixture command.
PROVIDERS = ("interactions",)
GATE_ACTIONS = ("hold", "release")
GATE_FIELDS = ("held", "waiting")
FILE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._ -]{0,99}")


def _check_files(files) -> list[str]:
    if not isinstance(files, list) or not files:
        return ["needs a nonempty files list"]
    names = [entry.get("name") if isinstance(entry, dict) else None for entry in files]
    problems = []
    for entry, name in zip(files, names):
        if not isinstance(name, str) or not FILE_NAME.fullmatch(name) or name.endswith("."):
            problems.append(f"has an invalid file name {name!r}")
        elif not isinstance(entry.get("text"), str):
            problems.append(f"file {name} needs text content")
    if len(set(names)) != len(names):
        problems.append("repeats a file name")
    return problems


def _check_condition_pair(value) -> list[str]:
    if not isinstance(value, dict) or set(value) - {"timeout"} != {"electron", "native"}:
        return ["needs a condition for each client"]
    return []


def check_step(step: dict, targets: dict[str, dict]) -> list[str]:
    """Problems with one step, worded to follow `step <kind> `."""
    kinds = [kind for kind in STEP_KINDS if kind in step]
    if len(kinds) != 1:
        return [f"must name exactly one step kind of {', '.join(STEP_KINDS)}: {sorted(step)}"]
    kind = kinds[0]
    value = step[kind]
    problems = []
    if kind == "wait":
        problems += _check_condition_pair(value)
    elif kind in ("clickTarget", "hover", "drop"):
        target = value.get("target") if kind == "drop" and isinstance(value, dict) else value
        if set(targets.get(target, {})) != {"electron", "native"}:
            problems.append(f"references undefined target {target}")
        if kind == "drop" and isinstance(value, dict):
            problems += _check_files(value.get("files"))
    elif kind == "drag":
        if not isinstance(value, dict) or set(value) != {"from", "to"}:
            problems.append("takes from and to targets")
        else:
            problems += [f"references undefined target {target}" for target in (value["from"], value["to"])
                         if set(targets.get(target, {})) != {"electron", "native"}]
    elif kind == "clickNamed":
        for client in ("electron", "native"):
            try:
                named_control(value, client)
            except (ValueError, TypeError, KeyError) as error:
                problems.append(str(error))
    elif kind == "key":
        try:
            parse_key(value)
        except ValueError as error:
            problems.append(str(error))
    elif kind == "paste":
        text = value.get("text") if isinstance(value, dict) else value
        if not isinstance(text, str) or not text:
            problems.append("needs nonempty text")
        if isinstance(value, dict) and "until" in value:
            problems += _check_condition_pair(value["until"])
    elif kind == "restart":
        if not isinstance(value, dict) or set(value) - {"after", "timeout"} or value.get("after", "signal") not in ("signal", "exit"):
            problems.append("takes {} or {\"after\": \"exit\"}")
    elif kind == "expect":
        problems += check_expectation(value)
    elif kind == "fault":
        for client in ("electron", "native"):
            try:
                fault_rule(value, client)
            except (ValueError, TypeError, KeyError) as error:
                problems.append(str(error))
    elif kind == "release":
        if not isinstance(value, str) or not value:
            problems.append("needs a fault id")
    elif kind == "pick":
        # CDP can answer an intercepted chooser but not cancel it as a user
        # does, so there is no cancel variant.
        if not isinstance(value, dict) or set(value) - {"files", "timeout"}:
            problems.append("takes files")
        else:
            problems += _check_files(value.get("files"))
            if isinstance(value.get("files"), list) and len(value["files"]) != 1:
                problems.append("picks exactly one file: the native chooser's location entry takes one path")
    elif kind == "ime":
        if not isinstance(value, dict):
            problems.append("takes compose or commit")
        elif "compose" in value:
            if not isinstance(value["compose"], str) or not re.fullmatch(r"[a-z]+", value["compose"]):
                problems.append("composes lower-case Latin keys for the pinyin engine")
            if set(value) - {"compose", "preedit"}:
                problems.append("compose takes only preedit")
            if "preedit" in value and (not isinstance(value["preedit"], str) or not value["preedit"]):
                problems.append("preedit is the engine's marked text")
        elif value.get("commit") == "candidate":
            if not isinstance(value.get("text"), str) or not value["text"]:
                problems.append("a candidate commit needs the expected text")
        elif value.get("commit") != "raw" or set(value) != {"commit"}:
            problems.append("commit is candidate (with text) or raw")
    elif kind == "gate":
        if value not in GATE_ACTIONS:
            problems.append("is hold or release")
    elif kind == "settle":
        if not isinstance(value, (int, float)) or value < 0:
            problems.append("needs seconds")
    elif kind == "type":
        if not isinstance(value, str):
            problems.append("needs text")
    return [f"step {kind} {problem}" for problem in problems]


def check_expectation(value) -> list[str]:
    if not isinstance(value, dict):
        return ["needs an api, file, git, fault or gate source"]
    sources = [source for source in ("api", "file", "git", "fault", "gate") if source in value]
    if len(sources) != 1:
        return ["needs exactly one of api, file, git, fault or gate"]
    source = sources[0]
    problems = []
    allowed = {source, "timeout", *PREDICATES, "where"}
    if source == "api":
        allowed.add("select")
        if not isinstance(value["api"], str) or not value["api"].startswith("/api/"):
            problems.append("api reads a path under /api/")
        if not isinstance(value.get("select", []), list):
            problems.append("select is a list of keys and indexes")
    elif source == "file":
        allowed.add("encoding")
        if not isinstance(value["file"], str) or not value["file"].startswith(("{fixture}/", "{project}/")):
            problems.append("file is under {fixture}/ or {project}/")
        if value.get("encoding", "utf-8") not in ("utf-8", "bytes"):
            problems.append("encoding is utf-8 or bytes")
    elif source == "git":
        allowed.add("cwd")
        arguments = value["git"]
        if (not isinstance(arguments, list) or not arguments or not all(isinstance(item, str) for item in arguments)
                or arguments[0] not in GIT_READS):
            problems.append(f"git runs one read command of {', '.join(GIT_READS)}")
        elif arguments[0] in GIT_LISTING_FLAGS and not set(arguments[1:]) <= GIT_LISTING_FLAGS[arguments[0]]:
            problems.append(f"git {arguments[0]} takes only {sorted(GIT_LISTING_FLAGS[arguments[0]])}")
        if not str(value.get("cwd", "{fixture}")).startswith(("{fixture}", "{project}")):
            problems.append("cwd is under {fixture} or {project}")
    elif source == "fault":
        allowed.add("hits")
        if not isinstance(value["fault"], str) or not isinstance(value.get("hits"), int):
            problems.append("fault names an id and the expected hits")
    elif source == "gate":
        if value["gate"] not in GATE_FIELDS:
            problems.append(f"gate reads one of {', '.join(GATE_FIELDS)}")
    if not any(predicate in value for predicate in (*PREDICATES, "hits")):
        problems.append(f"names a predicate of {', '.join(PREDICATES)}")
    if set(value) - allowed:
        problems.append(f"has unknown fields {sorted(set(value) - allowed)}")
    if not isinstance(value.get("timeout", 0), (int, float)):
        problems.append("timeout is seconds")
    return problems


def check_scenario(scenario: dict) -> list[str]:
    """Scenario-level options: fixture project and model pins."""
    problems = []
    if "fixture" in scenario and scenario["fixture"] not in FIXTURES:
        problems.append(f"unknown fixture {scenario['fixture']!r}")
    if "provider" in scenario and scenario["provider"] not in PROVIDERS:
        problems.append(f"unknown provider mode {scenario['provider']!r}")
    if "model" in scenario and (not isinstance(scenario["model"], str) or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.:#-]+", scenario["model"])):
        problems.append("model is provider/model")
    fault_ids = set()
    for step in scenario.get("steps", []):
        if "fault" in step and isinstance(step["fault"], dict):
            fault_ids.add(step["fault"].get("id"))
        if "release" in step and step["release"] not in fault_ids:
            problems.append(f"release names undeclared fault {step['release']}")
        if "expect" in step and isinstance(step["expect"], dict) and "fault" in step["expect"] and step["expect"]["fault"] not in fault_ids:
            problems.append(f"expect names undeclared fault {step['expect']['fault']}")
        expectation = step.get("expect")
        reads_fixture = isinstance(expectation, dict) and (
            "{fixture}" in json.dumps(expectation) or ("git" in expectation and "cwd" not in expectation))
        if reads_fixture and "fixture" not in scenario:
            problems.append("an expectation reads {fixture} without a scenario fixture")
    return problems
