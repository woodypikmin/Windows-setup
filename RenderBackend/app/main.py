from __future__ import annotations

import asyncio
import base64
import hashlib
import ipaddress
import json
import os
import shutil
import sqlite3
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import httpx
import jwt
from fastapi import FastAPI, Header, HTTPException, Request
from fastapi.responses import FileResponse
from pydantic import BaseModel, Field

BACKEND_VERSION = "rc6-render-no-domain"
APPLE = "https://api.appstoreconnect.apple.com"
BACKEND_ROOT = Path(__file__).resolve().parents[1]
RELEASE_CONFIG_PATH = Path(os.getenv("RELEASE_CONFIG_PATH", str(BACKEND_ROOT / "release.json"))).resolve()
JOB_DIR = Path(os.getenv("JOB_DIR", str(BACKEND_ROOT / "data" / "jobs"))).resolve()
DB_PATH = JOB_DIR / "jobs.sqlite3"
JOB_DIR.mkdir(parents=True, exist_ok=True)

BUNDLES = {
    "app": os.getenv("BUNDLE_ID_APP", "com.woodypikmin.pikminpilot"),
    "tunnel": os.getenv("BUNDLE_ID_TUNNEL", "com.woodypikmin.pikminpilot.tunnel"),
    "runner": os.getenv("BUNDLE_ID_RUNNER", "com.woodypikmin.pikminpilot.runner.xctrunner"),
}

app = FastAPI(title="Pikmin Pilot Cloud Provisioning", version=BACKEND_VERSION)


class ReleaseConfig(BaseModel):
    app_version: str = Field(min_length=1, max_length=64, pattern=r"^[A-Za-z0-9._+-]+$")
    baseline_repo: str = Field(pattern=r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
    release_tag: str = Field(min_length=1, max_length=128, pattern=r"^[A-Za-z0-9._+-]+$")
    ipa_asset: str = Field(min_length=5, max_length=255, pattern=r"^[^/\\]+\.ipa$")


class InstallRequest(BaseModel):
    udid: str = Field(min_length=8, max_length=128, pattern=r"^[A-Za-z0-9-]+$")
    platform: str = "IOS"
    # Legacy RC4 clients send this. It is intentionally ignored so the same old
    # runtime-download Setup can continue working after the server moves to a new app release.
    app_version: str | None = None
    setup_protocol: int | None = None


class InstallCreated(BaseModel):
    job_id: str
    status: str
    app_version: str


def load_release_config() -> ReleaseConfig:
    try:
        raw = json.loads(RELEASE_CONFIG_PATH.read_text(encoding="utf-8"))
        cfg = ReleaseConfig.model_validate(raw)
    except Exception as e:
        raise RuntimeError(f"invalid release config {RELEASE_CONFIG_PATH}: {e}") from e
    return cfg


class ApplePendingError(RuntimeError):
    pass


def db() -> sqlite3.Connection:
    con = sqlite3.connect(DB_PATH, timeout=30)
    con.row_factory = sqlite3.Row
    return con


def init_db() -> None:
    with db() as con:
        con.execute(
            """
            CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                udid TEXT NOT NULL,
                status TEXT NOT NULL,
                message TEXT,
                sha256 TEXT,
                callback_base TEXT,
                github_run_id TEXT,
                github_run_url TEXT,
                app_version TEXT,
                baseline_repo TEXT,
                release_tag TEXT,
                ipa_asset TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )
            """
        )
        # Safe migration from the RC4 MVP DB if the same JOB_DIR is reused.
        cols = {r["name"] for r in con.execute("PRAGMA table_info(jobs)")}
        for name, sql_type in (
            ("callback_base", "TEXT"),
            ("github_run_id", "TEXT"),
            ("github_run_url", "TEXT"),
            ("app_version", "TEXT"),
            ("baseline_repo", "TEXT"),
            ("release_tag", "TEXT"),
            ("ipa_asset", "TEXT"),
        ):
            if name not in cols:
                con.execute(f"ALTER TABLE jobs ADD COLUMN {name} {sql_type}")


init_db()


def update_job(
    job_id: str,
    status: str,
    message: str | None = None,
    sha256: str | None = None,
    github_run_id: str | None = None,
    github_run_url: str | None = None,
) -> None:
    with db() as con:
        con.execute(
            """
            UPDATE jobs
               SET status=?,
                   message=?,
                   sha256=COALESCE(?, sha256),
                   github_run_id=COALESCE(?, github_run_id),
                   github_run_url=COALESCE(?, github_run_url),
                   updated_at=?
             WHERE id=?
            """,
            (
                status,
                message,
                sha256,
                github_run_id,
                github_run_url,
                int(time.time()),
                job_id,
            ),
        )


def get_job(job_id: str) -> sqlite3.Row:
    with db() as con:
        row = con.execute("SELECT * FROM jobs WHERE id=?", (job_id,)).fetchone()
    if row is None:
        raise HTTPException(404, "job not found")
    return row


def job_path(job_id: str) -> Path:
    p = (JOB_DIR / job_id).resolve()
    if p.parent != JOB_DIR:
        raise RuntimeError("invalid job path")
    p.mkdir(parents=True, exist_ok=True)
    return p


def require_internal(auth: str | None) -> None:
    expected = os.getenv("BACKEND_WORKFLOW_TOKEN", "").strip()
    if not expected or auth != f"Bearer {expected}":
        raise HTTPException(401, "unauthorized")


def is_loopback_host(host: str) -> bool:
    h = host.split(":", 1)[0].strip("[]")
    if h.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(h).is_loopback
    except ValueError:
        return False


def callback_base_for_request(request: Request) -> str:
    configured = os.getenv("BACKEND_PUBLIC_URL", "").strip().rstrip("/")
    if configured:
        base = configured
    else:
        forwarded_proto = request.headers.get("x-forwarded-proto", "").split(",")[0].strip()
        forwarded_host = request.headers.get("x-forwarded-host", "").split(",")[0].strip()
        host = forwarded_host or request.headers.get("host", "").strip()
        scheme = forwarded_proto or request.url.scheme
        base = f"{scheme}://{host}".rstrip("/")

    parsed = urlparse(base)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise HTTPException(500, "cannot determine backend public URL")
    if parsed.scheme != "https" and not is_loopback_host(parsed.netloc):
        raise HTTPException(500, "public backend must use HTTPS")
    return base


def apple_jwt() -> str:
    key_id = os.environ["ASC_KEY_ID"].strip()
    issuer = os.environ["ASC_ISSUER_ID"].strip()
    pem = os.getenv("ASC_PRIVATE_KEY_PEM", "").strip()
    if pem:
        private_key = pem.replace("\\n", "\n")
    else:
        key_path = Path(os.getenv("ASC_PRIVATE_KEY_PATH", "/etc/secrets/AuthKey.p8")).expanduser().resolve()
        private_key = key_path.read_text(encoding="utf-8")
    now = int(time.time())
    payload = {
        "iss": issuer,
        "iat": now - 5,
        "exp": now + 15 * 60,
        "aud": "appstoreconnect-v1",
    }
    headers = {"alg": "ES256", "kid": key_id, "typ": "JWT"}
    return jwt.encode(payload, private_key, algorithm="ES256", headers=headers)


async def apple_request(
    method: str,
    path: str,
    *,
    params: dict[str, str] | None = None,
    body: dict[str, Any] | None = None,
    allow: set[int] | None = None,
) -> httpx.Response:
    allow = allow or {200, 201, 204}
    async with httpx.AsyncClient(timeout=60) as client:
        r = await client.request(
            method,
            APPLE + path,
            params=params,
            json=body,
            headers={
                "Authorization": f"Bearer {apple_jwt()}",
                "Content-Type": "application/json",
            },
        )
    if r.status_code not in allow:
        raise RuntimeError(f"Apple {method} {path} -> HTTP {r.status_code}: {r.text[:4000]}")
    return r


async def ensure_device(udid: str) -> tuple[str, bool]:
    r = await apple_request("GET", "/v1/devices", params={"filter[udid]": udid, "limit": "10"})
    data = r.json().get("data", [])
    if data:
        item = data[0]
        if item.get("attributes", {}).get("status") == "DISABLED":
            raise RuntimeError("Device exists in Apple Developer but is DISABLED")
        return item["id"], False

    suffix = udid[-8:] if len(udid) >= 8 else udid
    body = {
        "data": {
            "type": "devices",
            "attributes": {
                "name": f"PikminPilot-{suffix}",
                "platform": "IOS",
                "udid": udid,
            },
        }
    }
    r = await apple_request("POST", "/v1/devices", body=body, allow={201, 409})
    if r.status_code == 201:
        return r.json()["data"]["id"], True

    # A second request may have registered the same UDID at nearly the same time.
    r = await apple_request("GET", "/v1/devices", params={"filter[udid]": udid, "limit": "10"})
    data = r.json().get("data", [])
    if not data:
        raise RuntimeError(
            f"Apple returned HTTP 409 registering {udid}, but the device still cannot be found: {r.text[:1500]}"
        )
    return data[0]["id"], True


async def all_enabled_ios_device_ids() -> list[str]:
    r = await apple_request(
        "GET",
        "/v1/devices",
        params={"filter[platform]": "IOS", "filter[status]": "ENABLED", "limit": "200"},
    )
    return [x["id"] for x in r.json().get("data", [])]


def exact_bundle_matches(data: list[dict], identifier: str) -> list[dict]:
    # Apple's bundleIds identifier filter can return related/prefix matches.
    # Provisioning must bind to the explicit Bundle ID whose identifier exactly
    # matches the requested target, never to a tunnel/runner sibling.
    wanted = identifier.casefold()
    return [
        item
        for item in data
        if str(item.get("attributes", {}).get("identifier", "")).casefold() == wanted
    ]


async def bundle_resource_id(identifier: str) -> str:
    r = await apple_request(
        "GET", "/v1/bundleIds", params={"filter[identifier]": identifier, "limit": "50"}
    )
    returned = r.json().get("data", [])
    data = exact_bundle_matches(returned, identifier)
    if len(data) != 1:
        returned_identifiers = [
            str(x.get("attributes", {}).get("identifier", "<missing>")) for x in returned
        ]
        raise RuntimeError(
            f"Expected exactly one exact Apple bundleId for {identifier}; "
            f"found {len(data)} exact matches. API returned: {returned_identifiers}"
        )
    return data[0]["id"]


def parse_apple_time(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


async def resolve_certificate_id() -> str:
    configured = os.getenv("ASC_CERTIFICATE_ID", "").strip()
    params = {
        "filter[certificateType]": "DEVELOPMENT,IOS_DEVELOPMENT",
        "limit": "200",
    }
    r = await apple_request("GET", "/v1/certificates", params=params)
    certs = r.json().get("data", [])

    if configured:
        matches = [x for x in certs if x.get("id") == configured]
        if not matches:
            raise RuntimeError(
                "ASC_CERTIFICATE_ID does not match an active Development certificate returned by Apple"
            )
        return configured

    now = datetime.now(timezone.utc)
    active: list[dict[str, Any]] = []
    for x in certs:
        attrs = x.get("attributes", {})
        if attrs.get("activated") is False:
            continue
        exp = parse_apple_time(attrs.get("expirationDate"))
        if exp is not None and exp <= now:
            continue
        active.append(x)

    if len(active) == 1:
        return active[0]["id"]

    choices = []
    for x in active:
        a = x.get("attributes", {})
        choices.append(
            f"id={x.get('id')} displayName={a.get('displayName')} serial={a.get('serialNumber')} expires={a.get('expirationDate')}"
        )
    raise RuntimeError(
        "Could not auto-select the Apple Development certificate. "
        "Set ASC_CERTIFICATE_ID to the resource id matching your P12. Active candidates: "
        + (" | ".join(choices) if choices else "none")
    )


async def create_profile_once(
    job_id: str,
    label: str,
    bundle_identifier: str,
    certificate_id: str,
    device_ids: list[str],
) -> Path:
    bid = await bundle_resource_id(bundle_identifier)
    profile_name = f"PikminPilot RC5 {label} {int(time.time())} {job_id[:8]}"
    body = {
        "data": {
            "type": "profiles",
            "attributes": {"name": profile_name, "profileType": "IOS_APP_DEVELOPMENT"},
            "relationships": {
                "bundleId": {"data": {"type": "bundleIds", "id": bid}},
                "certificates": {"data": [{"type": "certificates", "id": certificate_id}]},
                "devices": {"data": [{"type": "devices", "id": x} for x in device_ids]},
            },
        }
    }
    r = await apple_request("POST", "/v1/profiles", body=body, allow={201, 409, 422})
    if r.status_code != 201:
        raise ApplePendingError(
            f"Apple profile create for {label} returned HTTP {r.status_code}: {r.text[:2500]}"
        )
    attrs = r.json()["data"]["attributes"]
    content = attrs.get("profileContent")
    if not content:
        raise RuntimeError(f"Apple profile create for {label} returned no profileContent")
    out = job_path(job_id) / f"{label}.mobileprovision"
    out.write_bytes(base64.b64decode(content))
    return out


async def create_all_profiles_with_retry(
    job_id: str,
    certificate_id: str,
    device_ids: list[str],
    newly_registered: bool,
) -> None:
    attempts = int(os.getenv("APPLE_PROFILE_CREATE_ATTEMPTS", "3"))
    delay = int(os.getenv("APPLE_PROFILE_RETRY_SECONDS", "8"))
    last: Exception | None = None
    for attempt in range(1, attempts + 1):
        try:
            for label in ("app", "tunnel", "runner"):
                await create_profile_once(
                    job_id, label, BUNDLES[label], certificate_id, device_ids
                )
            return
        except ApplePendingError as e:
            last = e
            # Remove partial local copies. Apple-side profiles created before the failing target
            # are harmless for this first validation phase; they use unique names.
            for label in ("app", "tunnel", "runner"):
                (job_path(job_id) / f"{label}.mobileprovision").unlink(missing_ok=True)
            if attempt < attempts:
                await asyncio.sleep(delay)

    prefix = (
        "The device was newly registered, but Apple has not yet allowed all Development profiles to be created. "
        if newly_registered
        else "Apple did not allow all Development profiles to be created. "
    )
    raise ApplePendingError(prefix + (str(last) if last else "unknown Apple profile error"))


async def github_dispatch(job_id: str, callback_base: str) -> None:
    owner = os.environ["GITHUB_OWNER"].strip()
    repo = os.environ["GITHUB_REPO_A"].strip()
    workflow = os.getenv("GITHUB_WORKFLOW", "rc5-provision-sign.yml").strip()
    ref = os.getenv("GITHUB_REF", "main").strip()
    token = os.environ["GITHUB_TOKEN"].strip()
    job = get_job(job_id)
    url = f"https://api.github.com/repos/{owner}/{repo}/actions/workflows/{workflow}/dispatches"
    body = {
        "ref": ref,
        "inputs": {
            "job_id": job_id,
            "backend_url": callback_base,
            "app_version": job["app_version"],
            "baseline_repo": job["baseline_repo"],
            "baseline_release_tag": job["release_tag"],
            "baseline_ipa_asset": job["ipa_asset"],
        },
    }
    headers = {
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": "2026-03-10",
        "User-Agent": "PikminPilot-Provisioner",
    }
    async with httpx.AsyncClient(timeout=60) as client:
        r = await client.post(url, headers=headers, json=body)
    if r.status_code not in {200, 204}:
        raise RuntimeError(
            f"GitHub workflow dispatch failed HTTP {r.status_code}: {r.text[:2500]}"
        )
    if r.status_code == 200 and r.content:
        try:
            data = r.json()
            update_job(
                job_id,
                "signing",
                "Profiles ready; A repo signing workflow started",
                github_run_id=str(data.get("workflow_run_id") or "") or None,
                github_run_url=data.get("html_url") or data.get("run_url"),
            )
        except Exception:
            pass


async def process_job(job_id: str, udid: str, callback_base: str) -> None:
    try:
        mock = os.getenv("MOCK_READY_IPA_PATH", "").strip()
        if mock:
            update_job(job_id, "signing", "Mock mode: using known-good IPA")
            src = Path(mock)
            if not src.is_file():
                raise RuntimeError(f"MOCK_READY_IPA_PATH not found: {src}")
            dst = job_path(job_id) / "PikminPilot.ipa"
            shutil.copy2(src, dst)
            digest = hashlib.sha256(dst.read_bytes()).hexdigest()
            update_job(job_id, "ready", "Mock transport test ready", digest)
            return

        update_job(job_id, "registering", "Checking Apple registered device")
        _device_id, newly_registered = await ensure_device(udid)
        update_job(
            job_id,
            "profiles",
            "Device registered; preparing Development profiles"
            if newly_registered
            else "Existing registered device found; preparing Development profiles",
        )
        devices = await all_enabled_ios_device_ids()
        if not devices:
            raise RuntimeError("Apple returned zero enabled iOS devices")
        certificate_id = await resolve_certificate_id()
        await create_all_profiles_with_retry(
            job_id, certificate_id, devices, newly_registered
        )
        update_job(job_id, "signing", "Profiles ready; dispatching A repo signing workflow")
        await github_dispatch(job_id, callback_base)
    except ApplePendingError as e:
        update_job(job_id, "apple_pending", str(e))
    except Exception as e:
        update_job(job_id, "failed", str(e))


@app.get("/healthz")
async def healthz() -> dict[str, str]:
    cfg = load_release_config()
    return {
        "ok": "true",
        "version": BACKEND_VERSION,
        "app_version": cfg.app_version,
        "release_tag": cfg.release_tag,
    }


@app.get("/api/v1/release")
async def current_release() -> dict[str, str]:
    cfg = load_release_config()
    return {
        "app_version": cfg.app_version,
        "release_tag": cfg.release_tag,
        "ipa_asset": cfg.ipa_asset,
    }


@app.post("/api/v1/install", response_model=InstallCreated, status_code=202)
async def create_install(req: InstallRequest, request: Request) -> InstallCreated:
    if req.platform != "IOS":
        raise HTTPException(400, "only IOS is supported")

    # Snapshot the release at job creation. A later release.json update cannot switch
    # an in-flight signing job to a different payload halfway through.
    cfg = load_release_config()
    callback_base = callback_base_for_request(request)
    job_id = uuid.uuid4().hex
    now = int(time.time())
    with db() as con:
        con.execute(
            """
            INSERT INTO jobs(
                id,udid,status,message,callback_base,
                app_version,baseline_repo,release_tag,ipa_asset,
                created_at,updated_at
            )
            VALUES(?,?,?,?,?,?,?,?,?,?,?)
            """,
            (
                job_id,
                req.udid,
                "queued",
                f"Provisioning queued for Pikmin Pilot {cfg.app_version}",
                callback_base,
                cfg.app_version,
                cfg.baseline_repo,
                cfg.release_tag,
                cfg.ipa_asset,
                now,
                now,
            ),
        )
    asyncio.create_task(process_job(job_id, req.udid, callback_base))
    return InstallCreated(job_id=job_id, status="queued", app_version=cfg.app_version)


@app.get("/api/v1/install/{job_id}")
async def install_status(job_id: str) -> dict[str, Any]:
    row = get_job(job_id)
    out: dict[str, Any] = {
        "status": row["status"],
        "message": row["message"],
        "app_version": row["app_version"],
    }
    if row["github_run_url"]:
        out["build_url"] = row["github_run_url"]
    if row["status"] == "ready":
        base = (row["callback_base"] or os.getenv("BACKEND_PUBLIC_URL", "")).rstrip("/")
        if not base:
            raise HTTPException(500, "job has no callback URL")
        out["download_url"] = f"{base}/api/v1/install/{job_id}/ipa"
        out["sha256"] = row["sha256"]
    return out


@app.get("/api/v1/install/{job_id}/ipa")
async def download_ipa(job_id: str) -> FileResponse:
    row = get_job(job_id)
    if row["status"] != "ready":
        raise HTTPException(409, "IPA is not ready")
    path = job_path(job_id) / "PikminPilot.ipa"
    if not path.is_file():
        raise HTTPException(500, "result missing")
    return FileResponse(
        path,
        media_type="application/octet-stream",
        filename=f"PikminPilot-{row['app_version'] or 'current'}.ipa",
    )


@app.get("/internal/jobs/{job_id}/profiles/{label}")
async def internal_profile(
    job_id: str,
    label: str,
    authorization: str | None = Header(default=None),
) -> FileResponse:
    require_internal(authorization)
    if label not in {"app", "tunnel", "runner"}:
        raise HTTPException(404, "unknown profile")
    get_job(job_id)
    path = job_path(job_id) / f"{label}.mobileprovision"
    if not path.is_file():
        raise HTTPException(404, "profile not ready")
    return FileResponse(path, media_type="application/octet-stream", filename=path.name)


@app.put("/internal/jobs/{job_id}/result")
async def internal_upload_result(
    job_id: str,
    request: Request,
    authorization: str | None = Header(default=None),
    x_ipa_sha256: str | None = Header(default=None),
) -> dict[str, str]:
    require_internal(authorization)
    get_job(job_id)

    max_bytes = int(os.getenv("MAX_IPA_BYTES", str(150 * 1024 * 1024)))
    content_length = request.headers.get("content-length")
    if content_length and int(content_length) > max_bytes:
        raise HTTPException(413, "IPA too large")
    data = await request.body()
    if len(data) < 1024 * 1024:
        raise HTTPException(400, "IPA too small")
    if len(data) > max_bytes:
        raise HTTPException(413, "IPA too large")

    got = hashlib.sha256(data).hexdigest()
    if x_ipa_sha256 and got.lower() != x_ipa_sha256.lower():
        raise HTTPException(400, f"SHA256 mismatch: header={x_ipa_sha256} got={got}")
    out = job_path(job_id) / "PikminPilot.ipa"
    out.write_bytes(data)
    update_job(job_id, "ready", "Signed IPA ready", got)
    return {"ok": "true", "sha256": got}


@app.post("/internal/jobs/{job_id}/failed")
async def internal_failed(
    job_id: str,
    request: Request,
    authorization: str | None = Header(default=None),
) -> dict[str, str]:
    require_internal(authorization)
    get_job(job_id)
    try:
        body = await request.json()
        message = str(body.get("message", "A repo signing workflow failed"))[:4000]
    except Exception:
        message = "A repo signing workflow failed"
    update_job(job_id, "failed", message)
    return {"ok": "true"}
