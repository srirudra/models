#!/usr/bin/env node
import { runCli } from "../dist/cli/main.js";

const code = await runCli();
process.exitCode = code;
