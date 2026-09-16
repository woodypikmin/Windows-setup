from __future__ import annotations

import asyncio
import os
import sys

from main import BUNDLES, RELEASE_CONFIG_PATH, apple_request, exact_bundle_matches, load_release_config, resolve_certificate_id


async def apple_check() -> int:
    print("Apple Team API check")
    print("====================")
    try:
        r = await apple_request(
            "GET",
            "/v1/certificates",
            params={
                "filter[certificateType]": "DEVELOPMENT,IOS_DEVELOPMENT",
                "limit": "200",
            },
        )
    except Exception as e:
        print(f"FAIL: {e}")
        return 2

    certs = r.json().get("data", [])
    print("\nDevelopment certificates:")
    if not certs:
        print("  (none)")
    for x in certs:
        a = x.get("attributes", {})
        print(
            f"  id={x['id']} type={a.get('certificateType')} "
            f"name={a.get('displayName')} serial={a.get('serialNumber')} "
            f"activated={a.get('activated')} expires={a.get('expirationDate')}"
        )

    print("\nRequired bundle IDs:")
    ok = True
    for label, ident in BUNDLES.items():
        r = await apple_request(
            "GET", "/v1/bundleIds", params={"filter[identifier]": ident, "limit": "10"}
        )
        returned = r.json().get("data", [])
        data = exact_bundle_matches(returned, ident)
        if len(data) == 1:
            print(f"  OK {label}: {ident} -> {data[0]['id']}")
        else:
            ok = False
            returned_identifiers = [
                str(x.get("attributes", {}).get("identifier", "<missing>"))
                for x in returned
            ]
            print(
                f"  FAIL {label}: {ident} -> found {len(data)} exact matches; "
                f"API returned {returned_identifiers}"
            )

    print("\nCertificate selection:")
    try:
        cid = await resolve_certificate_id()
        print(f"  OK selected certificate id={cid}")
    except Exception as e:
        ok = False
        print(f"  ACTION REQUIRED: {e}")

    return 0 if ok else 3


async def github_check() -> int:
    import httpx

    owner = os.getenv("GITHUB_OWNER", "").strip()
    repo = os.getenv("GITHUB_REPO_A", "").strip()
    workflow = os.getenv("GITHUB_WORKFLOW", "rc5-provision-sign.yml").strip()
    token = os.getenv("GITHUB_TOKEN", "").strip()
    if not owner or not repo or not token:
        print("FAIL: set GITHUB_OWNER, GITHUB_REPO_A and GITHUB_TOKEN")
        return 2
    url = f"https://api.github.com/repos/{owner}/{repo}/actions/workflows/{workflow}"
    headers = {
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": "2026-03-10",
    }
    async with httpx.AsyncClient(timeout=30) as c:
        r = await c.get(url, headers=headers)
    if r.status_code != 200:
        print(f"FAIL: GitHub HTTP {r.status_code}: {r.text[:2000]}")
        return 2
    j = r.json()
    print(f"OK workflow id={j.get('id')} name={j.get('name')} state={j.get('state')}")
    return 0


async def release_check() -> int:
    print("Dynamic release config check")
    print("============================")
    try:
        cfg = load_release_config()
    except Exception as e:
        print(f"FAIL: {e}")
        return 2
    print(f"OK config: {RELEASE_CONFIG_PATH}")
    print(f"  app_version:   {cfg.app_version}")
    print(f"  baseline_repo: {cfg.baseline_repo}")
    print(f"  release_tag:   {cfg.release_tag}")
    print(f"  ipa_asset:     {cfg.ipa_asset}")
    api = f"https://api.github.com/repos/{cfg.baseline_repo}/releases/tags/{cfg.release_tag}"
    try:
        import httpx
        async with httpx.AsyncClient(timeout=30) as c:
            r = await c.get(api, headers={"Accept": "application/vnd.github+json", "User-Agent": "PikminPilot-OwnerCheck"})
        if r.status_code != 200:
            print(f"FAIL release tag lookup: GitHub HTTP {r.status_code}: {r.text[:1000]}")
            return 3
        release = r.json()
        assets = {str(a.get("name")): a for a in release.get("assets", [])}
        asset = assets.get(cfg.ipa_asset)
        if asset is None:
            print(f"FAIL release asset: {cfg.ipa_asset} not found; available={list(assets)}")
            return 3
        print(f"OK public release asset found (size={asset.get('size', 'unknown')} bytes)")
    except Exception as e:
        print(f"FAIL release asset check: {e}")
        return 3
    print("  New install jobs snapshot these values, so editing release.json only affects future jobs.")
    return 0


async def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] not in {"apple-check", "github-check", "release-check"}:
        print("Usage: python owner_cli.py apple-check|github-check|release-check")
        return 1
    if sys.argv[1] == "apple-check":
        return await apple_check()
    if sys.argv[1] == "github-check":
        return await github_check()
    return await release_check()


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
