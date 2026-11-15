#!/usr/bin/env python3
"""Bulk-load a hand-written parts catalogue (YAML + photos) into InvenTree via its REST API.

Idempotent and additive: a part is matched by IPN (= the yaml `id`). Existing parts,
stock, parameters and attachments are left alone, so edits made in the InvenTree UI are
never overwritten. Add entries to the yaml and re-run to load just those.

    inventree_import.py parts.yaml [--photo-root DIR]

Photo paths in the yaml are relative to --photo-root (default: the yaml's directory).
Needs only `requests` + `pyyaml` (both present in the inventree/inventree image). Env:
  INVENTREE_API_URL                 e.g. http://inventree-server:8000
  INVENTREE_TOKEN                   API token, or
  INVENTREE_USER / INVENTREE_PASSWORD
See README.md and example-parts.yaml for the file format.
"""

import argparse
import os
import sys
from pathlib import Path

import requests
import yaml

API = os.environ.get("INVENTREE_API_URL", "http://localhost:8000").rstrip("/") + "/api"
ROOT = Path(".")
S = requests.Session()
S.headers["Accept"] = "application/json"


def authenticate():
    if os.environ.get("INVENTREE_TOKEN"):
        S.headers["Authorization"] = f"Token {os.environ['INVENTREE_TOKEN']}"
    elif os.environ.get("INVENTREE_USER"):
        S.auth = (os.environ["INVENTREE_USER"], os.environ.get("INVENTREE_PASSWORD", ""))
    else:
        sys.exit("set INVENTREE_TOKEN, or INVENTREE_USER and INVENTREE_PASSWORD")


# yaml category slug -> InvenTree category path ("Parent/Child" nests).
# Override or extend with a top-level `categories:` mapping in the yaml.
CATEGORY_NAMES = {
    "microcontroller": "Microcontrollers",
    "computer": "Single-board computers",
    "wireless-wifi": "Wireless/Wi-Fi",
    "wireless-bluetooth": "Wireless/Bluetooth",
    "wireless-rf": "Wireless/RF (sub-GHz)",
    "networking": "Networking",
    "interface": "Interfaces & adapters",
    "actuator": "Actuators & relays",
    "power": "Power",
    "sensor": "Sensors",
    "display": "Displays",
    "input": "Input",
    "component": "Components",
    "cable": "Cables",
    "tool": "Tools",
    "hardware": "Hardware",
    "kit": "Kits",
    "misc": "Misc",
    "unknown": "Unidentified",
}
CORE_KEYS = {"id", "name", "category", "qty", "location", "photos", "notes"}


def call(method, path, **kw):
    r = S.request(method, f"{API}/{path.lstrip('/')}", **kw)
    if not r.ok:
        sys.exit(f"{method} {path} -> {r.status_code}: {r.text[:500]}")
    return r.json() if r.content else None


def results(data):
    return data["results"] if isinstance(data, dict) and "results" in data else data


def get_or_create(path, match, create):
    found = results(call("GET", path, params=match))
    if found:
        return found[0], False
    return call("POST", path, json=create), True


_CATEGORY_PKS = {}
_TEMPLATE_PKS = {}


def ensure_category(slug):
    cache = _CATEGORY_PKS
    full = CATEGORY_NAMES.get(slug, slug.replace("-", " ").title())
    if full in cache:
        return cache[full]
    parent = None
    for name in full.split("/"):
        params = {"name": name, "parent": parent} if parent else {"name": name, "top_level": "true"}
        cat, _ = get_or_create("part/category/", params, {"name": name, "parent": parent})
        parent = cat["pk"]
    cache[full] = parent
    return parent


def ensure_locations(locs):
    ids = {}
    pending = dict(locs)
    while pending:
        for code, info in list(pending.items()):
            parent_code = info.get("parent")
            if parent_code and parent_code not in ids:
                continue
            parent = ids.get(parent_code)
            params = {"name": code, "parent": parent} if parent else {"name": code, "top_level": "true"}
            loc, created = get_or_create(
                "stock/location/",
                params,
                {"name": code, "parent": parent, "description": (info.get("description") or "")[:250]},
            )
            ids[code] = loc["pk"]
            print(f"location {code}: {'created' if created else 'exists'} (pk {loc['pk']})")
            del pending[code]
    return ids


def ensure_template(name):
    cache = _TEMPLATE_PKS
    if name not in cache:
        t, _ = get_or_create("parameter/template/", {"name": name}, {"name": name, "model_type": "part.part"})
        cache[name] = t["pk"]
    return cache[name]


def notes_md(p):
    out = []
    if p.get("notes"):
        out.append(str(p["notes"]).strip())
    out.append(f"\n---\nImported from `{SOURCE}` (id `{p['id']}`, confidence **{p.get('confidence', '?')}**).")
    if p.get("photos"):
        out.append("Photos: " + ", ".join(f"`{x}`" for x in p["photos"]))
    return "\n\n".join(out)


def attach_photos(part_pk, photos):
    have = {a["filename"] for a in results(call("GET", "attachment/", params={"model_type": "part", "model_id": part_pk}))}
    for rel in photos:
        f = ROOT / rel
        if f.name in have:
            continue
        with f.open("rb") as fh:
            call(
                "POST", "attachment/", data={"model_type": "part", "model_id": part_pk, "comment": rel}, files={"attachment": (f.name, fh)}
            )


SOURCE = "parts.yaml"


def main():
    global ROOT, SOURCE
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("parts_file", type=Path, help="parts catalogue (yaml)")
    ap.add_argument("--photo-root", type=Path, help="directory photo paths are relative to (default: the yaml's directory)")
    args = ap.parse_args()
    authenticate()
    ROOT = args.photo_root or args.parts_file.parent
    SOURCE = args.parts_file.name

    data = yaml.safe_load(args.parts_file.read_text())
    CATEGORY_NAMES.update(data.get("categories") or {})
    loc_ids = ensure_locations(data.get("locations") or {})

    missing = [ph for p in data["parts"] for ph in p.get("photos", []) if not (ROOT / ph).is_file()]
    if missing:
        sys.exit(f"missing photo files: {missing}")

    created = 0
    for p in data["parts"]:
        existing = results(call("GET", "part/", params={"IPN": p["id"]}))
        if existing:
            part = existing[0]
        else:
            name = p["name"] if len(p["name"]) <= 100 else p["name"][:97] + "..."
            part = call(
                "POST",
                "part/",
                json={
                    "name": name,
                    "IPN": p["id"],
                    "description": p["name"][:250],
                    "category": ensure_category(p["category"]),
                    "default_location": loc_ids[p["location"]],
                    "component": True,
                    "purchaseable": True,
                    "notes": notes_md(p),
                },
            )
            created += 1

        pk = part["pk"]
        # parameters: every non-core scalar key (chip, sku, output, confidence, ...)
        have = {x["template"] for x in results(call("GET", "parameter/", params={"model_type": "part.part", "model_id": pk}))}
        for k, v in p.items():
            if k in CORE_KEYS or isinstance(v, (list, dict)) or v is None:
                continue
            t = ensure_template(k.replace("_", " ").title())
            if t not in have:
                call("POST", "parameter/", json={"template": t, "model_type": "part.part", "model_id": pk, "data": str(v)})

        if not results(call("GET", "stock/", params={"part": pk})):
            call("POST", "stock/", json={"part": pk, "location": loc_ids[p["location"]], "quantity": p["qty"]})

        photos = p.get("photos") or []
        if photos and not part.get("image"):
            with (ROOT / photos[0]).open("rb") as fh:
                call("PATCH", f"part/{pk}/", files={"image": (Path(photos[0]).name, fh)})
        attach_photos(pk, photos)
        print(f"{'NEW ' if not existing else 'ok  '} {p['id']}")

    print(f"done: {created} new, {len(data['parts']) - created} already present")


if __name__ == "__main__":
    main()
