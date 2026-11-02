#!/usr/bin/env node
// Reads a Kratu progress feed and reports each learner's cumulative mastered
// count to the fund contract. Dry-run by default; pass --send to submit.
// Usage: node sync.js <contract-id> <oracle-account-id> [--send] [feed.json]
const fs = require("fs");
const { execFileSync } = require("child_process");

const args = process.argv.slice(2).filter((a) => a !== "--send");
const send = process.argv.includes("--send");
const [contract, oracle, feedPath = `${__dirname}/feed.json`] = args;
if (!contract || !oracle) {
  console.error("usage: node sync.js <contract-id> <oracle-account-id> [--send] [feed.json]");
  process.exit(1);
}

const feed = JSON.parse(fs.readFileSync(feedPath, "utf8"));
// The contract rejects jumps larger than this in one report, so catch-up is staged.
const MAX_STEP = 150;
const steps = (n) => {
  const out = [];
  for (let v = MAX_STEP; v < n; v += MAX_STEP) out.push(v);
  return [...out, n];
};

for (const [centre, { learners }] of Object.entries(feed.centres)) {
  for (const [hash, total] of Object.entries(learners)) {
   for (const mastered of steps(total)) {
    const payload = JSON.stringify({ centre_id: centre, learner_hash: hash, items_mastered: mastered });
    const cmd = ["contract", "call-function", "as-transaction", contract, "report_progress",
      "json-args", payload, "prepaid-gas", "30 Tgas", "attached-deposit", "0 NEAR",
      "sign-as", oracle, "network-config", "testnet", "sign-with-keychain", "send"];
    if (!send) {
      console.log(`[dry-run] ${centre}/${hash}: ${mastered} items -> near ${cmd.join(" ")}`);
      continue;
    }
    console.log(`reporting ${centre}/${hash}: ${mastered}`);
    execFileSync("near", cmd, { stdio: "inherit" });
   }
  }
}
