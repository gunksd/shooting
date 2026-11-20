#!/usr/bin/env python3
"""Replays the real SLERF launch on an anvil fork of BSC and drives the real `shooter` binary against it.

  data source : an archive BSC RPC given as BSC_ARCHIVE_RPC (state at block 134305832 is required)
  fork block  : 134305832, the block right before the launch
  launch tx   : 0x13338bb0e2351637908b6806c8b9b84e35f6b4ce281589769bf6df32ea864c17
                replayed with the same sender, calldata, value, gas limit and gas price

Covered end to end with the release binary: deploy, plan-task, the `run` loop (poller, launch
detection, probe arming, signing, broadcast, receipts, budget), status, and withdraw.

This proves token compatibility and program logic on real mainnet state. Transaction ordering here
is anvil's (gas price descending), not a mainnet block producer's, so it is not a success-rate figure.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "shooter/target/release/shooter")
ARTIFACT = os.path.join(ROOT, "contracts/artifacts/SellExecutor.json")
ARCHIVE = os.environ.get("BSC_ARCHIVE_RPC") or sys.exit("set BSC_ARCHIVE_RPC to an archive BSC endpoint")
PORT = int(os.environ.get("ANVIL_PORT", "18700"))
URL = f"http://127.0.0.1:{PORT}"
SETTLE = float(os.environ.get("SETTLE_SECS", "1.5"))  # time the daemon gets to react after each mined block

LAUNCH_BLOCK = 134305833
TOKEN = "0xEbE3069bC7B5F1602De421f7a1089759F15AB636"
SALE = "0x13406cE228a4c7A32699b384ECe4da874dc407D1"
OPENER = "0x40CCd75565F9f98ea64b5252F1781981410D0b27"
HOLDER = "0x14B61F3E84C0ee4efA978007404e45bb214f7018"  # real pre-launch holder of 500,000 SLERF
WBNB = "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c"
USDT = "0x55d398326f99059fF775485246999027B3197955"
ROUTER = "0x10ED43C718714eb63d5aA57B78B54704E256024E"
FACTORY = "0xcA143Ce32Fe78f1f7019d7d551a6402fC5350c73"
INIT_HASH = "0x00fb7f630766e6a796048ea87d01acd3068e8ff67d078148a3fa3f4a84f69bd5"
PRICE_PAIR = "0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE"
OPEN_DATA = "0xadbbadec" + "2".rjust(64, "0") + "40".rjust(64, "0") + "0".rjust(64, "0")
OPEN_VALUE, OPEN_GAS, OPEN_GAS_PRICE = 6 * 10**15, 4530864, 93143630
SOLD_SELECTOR = "0x3a98ef39"  # shares sold: 7998 before the launch block, 8000 after
RECIPIENT = "0x00000000000000000000000000000000000000Aa"
WITHDRAW_TO = "0x00000000000000000000000000000000000000Bb"
COST_WEI = 15 * 10**15  # 500,000 tokens = 5 shares x 0.003 BNB
EXPECTED_OUT = 44775840248380600  # what the first real seller received for the same amount

_open = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def rpc(method, params=None):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode()
    req = urllib.request.Request(URL, body, {"content-type": "application/json"})
    out = json.load(_open.open(req, timeout=180))
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error']}")
    return out["result"]


def call(to, data):
    return rpc("eth_call", [{"to": to, "data": data}, "latest"])


def pad(addr):
    return addr.lower().replace("0x", "").rjust(64, "0")


def shooter(cfg, *args):
    r = subprocess.run([BIN, "--config", cfg, *args], capture_output=True, text=True, timeout=180)
    lines = [l for l in r.stdout.strip().splitlines() if l.startswith("{")]
    if r.returncode != 0 or not lines:
        raise RuntimeError(f"shooter {' '.join(args[:1])} failed: {r.stderr.strip() or r.stdout.strip()}")
    return json.loads(lines[-1])


def wait_receipt(tx):
    for _ in range(600):
        r = rpc("eth_getTransactionReceipt", [tx])
        if r:
            return r
        time.sleep(0.1)
    raise RuntimeError(f"{tx} not mined")


def mine():
    rpc("evm_mine")
    time.sleep(SETTLE)


def config(path, workdir, task=""):
    with open(path, "w") as f:
        f.write(f'''dry_run = false
chain_id = 56
db_path = "{workdir}/shooter.db"

[rpc]
http = ["{URL}"]

[executor]
key_file = "{workdir}/executor.key"

[budget]
usd_micros = 500000
min_price_micros = 100000000
max_price_micros = 5000000000
price_pair = "{PRICE_PAIR}"
price_stable = "{USDT}"

[[channels]]
name = "public"
kind = "public"
url = "{URL}"
{task}''')


def task_toml(task_id, contract, amount, min_out, mode):
    spray = mode == "pre_armed_spray"
    probe = f'arm_probe = {{ to = "{SALE}", data = "{SOLD_SELECTOR}", min_value = "0x1f36" }}\n' if spray else ""
    return f'''
[[tasks]]
task_id = "{task_id}"
executor_contract = "{contract}"
owner = "{HOLDER}"
token_in = "{TOKEN}"
token_out = "{WBNB}"
factory = "{FACTORY}"
pair_init_code_hash = "{INIT_HASH}"
amount_in = "{hex(amount)}"
min_out = "{hex(min_out)}"
mode = "{mode}"
gas_limit = 300000
gas_price_wei = {50000000 if spray else 1000000000}
channels = ["public"]
max_attempts = {30 if spray else 2}
window_blocks = {30 if spray else 3}
require_sim_ok = {"false" if spray else "true"}
spray_per_block = {1 if spray else 0}
{probe}'''


def scenario(tmp, key, mode, task_id, contract, amount, min_out):
    work = os.path.join(tmp, mode)
    os.makedirs(work)
    shutil.copy(key, os.path.join(work, "executor.key"))
    cfg = os.path.join(work, "config.toml")
    config(cfg, work, task_toml(task_id, contract, amount, min_out, mode))
    log = open(os.path.join(work, "daemon.log"), "w")
    daemon = subprocess.Popen([BIN, "--config", cfg, "run"], stdout=log, stderr=subprocess.STDOUT)
    try:
        time.sleep(3)
        if daemon.poll() is not None:
            raise RuntimeError("daemon exited early:\n" + open(log.name).read()[-2000:])
        for _ in range(3):  # blocks before the launch: spray attempts land here and must fail cheaply
            mine()
        launch = rpc("eth_sendTransaction", [{"from": OPENER, "to": SALE, "data": OPEN_DATA, "value": hex(OPEN_VALUE), "gas": hex(OPEN_GAS), "gasPrice": hex(OPEN_GAS_PRICE)}])
        mine()
        lr = wait_receipt(launch)
        assert lr["status"] == "0x1", "replayed launch transaction failed"
        for _ in range(3):
            mine()
    finally:
        daemon.send_signal(signal.SIGINT)
        try:
            daemon.wait(15)
        except subprocess.TimeoutExpired:
            daemon.kill()
        log.close()
    st = shooter(cfg, "status")
    t = st["tasks"][0]
    if t["state"] != "confirmed":
        raise AssertionError(f"{mode}: task is {t['state']}\n" + open(log.name).read()[-3000:])
    res = json.loads(t["result"])
    metrics = {}
    for name, value in t["metrics"]:
        metrics.setdefault(name, []).append(value)
    got = int(call(WBNB, "0x70a08231" + pad(RECIPIENT)), 16)
    launch_block, launch_index = int(lr["blockNumber"], 16), int(lr["transactionIndex"], 16)
    out = {
        "mode": mode,
        "launch": (launch_block, launch_index),
        "sale": (res["block"], res["tx_index"]),
        "received_wei": int(res["received"]),
        "recipient_wbnb_wei": got,
        "multiple": int(res["received"]) / COST_WEI,
        "attempts": t["attempts"],
        "attempt_states": sorted(a["state"] for a in st["attempts"]),
        "sale_gas_used": res["gas_used"],
        "all_gas_used": metrics.get("gas_used"),
        "spent_wei": int(t["spent_wei"]),
        "cap_wei": int(t["cap_wei"]),
        "sign_us": metrics.get("sign_us"),
        "submit_ms": metrics.get("submit_ms:public"),
        "decision_ms": metrics.get("decision_ms"),
    }
    assert got == int(res["received"]) == EXPECTED_OUT, f"{mode}: received {got}, expected {EXPECTED_OUT}"
    assert int(call(TOKEN, "0x70a08231" + pad(HOLDER)), 16) == 0, "owner still holds tokens"
    assert int(call(TOKEN, "0x70a08231" + pad(contract)), 16) == 0, "executor contract holds tokens"
    assert out["spent_wei"] <= out["cap_wei"], "budget exceeded"
    if mode == "pre_armed_spray":
        assert out["sale"][0] == launch_block and out["sale"][1] > launch_index, f"not same block after launch: {out}"
        assert "included_failed" in out["attempt_states"], "expected at least one cheap pre-launch attempt"
    else:
        assert out["sale"][0] == launch_block + 1, f"not next block: {out}"
        assert out["attempts"] == 1, "next-block mode must send exactly one transaction here"
    return out


def main():
    for need in (BIN, ARTIFACT):
        if not os.path.exists(need):
            sys.exit(f"missing {need}: build first (cargo build --release, forge build)")
    tmp = tempfile.mkdtemp(prefix="shooter-fork-")
    anvil = subprocess.Popen(
        ["anvil", "--fork-url", ARCHIVE, "--fork-block-number", str(LAUNCH_BLOCK - 1), "--port", str(PORT), "--block-base-fee-per-gas", "0", "--retries", "10", "--timeout", "60000", "--silent"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(300):
            try:
                rpc("eth_blockNumber")
                break
            except Exception:
                time.sleep(0.2)
        else:
            sys.exit("anvil did not start")

        key = os.path.join(tmp, "executor.key")
        with open(os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as f:
            f.write("0x" + os.urandom(32).hex() + "\n")
        base = os.path.join(tmp, "base.toml")
        config(base, tmp)
        executor = shooter(base, "address")["address"]
        rpc("anvil_setBalance", [executor, hex(10**16)])

        dep = shooter(base, "deploy", "--artifact", ARTIFACT)
        contract = dep["address"]
        print(f"deployed SellExecutor {contract}  cost {int(dep['cost_wei']) / 1e18:.6f} BNB")

        amount = int(call(TOKEN, "0x70a08231" + pad(HOLDER)), 16)
        min_out = 2 * COST_WEI  # owner's floor: 2x cost
        plan = shooter(base, "plan-task", "--executor-contract", contract, "--owner", HOLDER, "--token", TOKEN, "--token-out", WBNB,
                       "--router", ROUTER, "--factory", FACTORY, "--recipient", RECIPIENT, "--amount", str(amount), "--min-out-floor", str(min_out))
        assert int(call(TOKEN, "0xffb54a99"), 16) == 0, "trading should still be closed at the fork block"
        for who in (HOLDER, OPENER):
            rpc("anvil_impersonateAccount", [who])
            rpc("anvil_setBalance", [who, hex(10**18)])
        for name in ("approve", "create_task"):  # what the owner wallet does, before trading opens
            r = wait_receipt(rpc("eth_sendTransaction", [{"from": HOLDER, "to": plan[name]["to"], "data": plan[name]["data"]}]))
            assert r["status"] == "0x1", f"{name} failed before launch"
            print(f"owner {name}: gas {int(r['gasUsed'], 16)}")
        print(f"task {plan['task_id']}  amount {amount / 1e18:,.0f} SLERF  floor {min_out / 1e18} BNB  shares sold {int(call(SALE, SOLD_SELECTOR), 16)}")

        rpc("evm_setAutomine", [False])
        results = []
        for mode in ("pre_armed_spray", "next_block_best_effort"):
            snap = rpc("evm_snapshot")
            results.append(scenario(tmp, key, mode, plan["task_id"], contract, amount, min_out))
            assert rpc("evm_revert", [snap])
        for r in results:
            print(json.dumps(r))

        rpc("evm_setAutomine", [True])
        before = int(rpc("eth_getBalance", [executor, "latest"]), 16)
        w = shooter(base, "withdraw", WITHDRAW_TO)
        after = int(rpc("eth_getBalance", [executor, "latest"]), 16)
        back = int(rpc("eth_getBalance", [WITHDRAW_TO, "latest"]), 16)
        assert w["confirmed"] and after == 0 and back == int(w["amount_wei"]), f"withdraw mismatch: {w} left {after} arrived {back}"
        print(f"withdraw: executor had {before / 1e18:.6f} BNB, {back / 1e18:.6f} arrived, {after} wei left, fee {(before - back) / 1e18:.9f} BNB")
        print("ALL CHECKS PASSED")
    finally:
        anvil.kill()
        anvil.wait()
        if not os.environ.get("KEEP_TMP"):
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print("kept", tmp)


if __name__ == "__main__":
    main()
