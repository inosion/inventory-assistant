# yaml-import

Bulk-load an existing, hand-written parts catalogue into InvenTree. This is useful when you already have a list
(or have had an AI assistant catalogue a pile of photos into YAML) and want it all in InvenTree in one go.

- **Idempotent and additive.** Parts are matched by IPN (the yaml `id`). Existing parts, stock, parameters and
  attachments are never modified, so it is safe to re-run after editing things in the InvenTree UI.
- Locations are created with nesting (`parent:`), and categories with nesting (`Parent/Child`).
- Extra scalar keys (`chip`, `sku`, `output`, …) become part **parameters**.
- The first photo becomes the part image. All photos are attached.

See [example-parts.yaml](example-parts.yaml) for the format.

## Run

```sh
pip install -r requirements.txt
export INVENTREE_API_URL=https://inventree.example.lan:8443
export INVENTREE_TOKEN=...            # or INVENTREE_USER + INVENTREE_PASSWORD
./inventree_import.py my-parts.yaml --photo-root .
```

Or with no local Python, inside the InvenTree image on the compose network (it already has `requests` and `pyyaml`):

```sh
docker run --rm --network inventree_default -v "$PWD:/work:ro" -w /work --entrypoint python3 \
  -e INVENTREE_API_URL=http://inventree-server:8000 -e INVENTREE_TOKEN \
  inventree/inventree:1.7.1 /work/inventree_import.py /work/my-parts.yaml
```
