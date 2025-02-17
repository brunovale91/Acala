import { TestProvider } from "@acala-network/bodhi";
import { WsProvider } from "@polkadot/api";
import { spawn, ChildProcess } from "child_process";
import chaiAsPromised from "chai-as-promised";
import chai from "chai";

chai.use(chaiAsPromised);

export const P2P_PORT = 19931;
export const RPC_PORT = 19932;
export const WS_PORT = 19933;

export const DISPLAY_LOG = process.env.ACALA_LOG || false;
export const ACALA_LOG = process.env.ACALA_LOG || "info";
export const ACALA_BUILD = process.env.ACALA_BUILD || "debug";

export const BINARY_PATH = `../target/${ACALA_BUILD}/acala`;
export const SPAWNING_TIME = 60000;

export async function startAcalaNode(): Promise<{ provider: TestProvider; binary: ChildProcess }> {
	const cmd = BINARY_PATH;
	const args = [
		`--dev`,
		`-lruntime=debug`,
		`-levm=debug`,
		`--instant-sealing`,
		`--execution=native`, // Faster execution using native
		`--no-telemetry`,
		`--no-prometheus`,
		`--port=${P2P_PORT}`,
		`--rpc-port=${RPC_PORT}`,
		`--rpc-external`,
		`--ws-port=${WS_PORT}`,
		`--ws-external`,
		`--rpc-cors=all`,
		`--rpc-methods=unsafe`,
		`--tmp`,
	];
	const binary = spawn(cmd, args);

	binary.on("error", (err) => {
		if ((err as any).errno == "ENOENT") {
			console.error(
				`\x1b[31mMissing Acala binary (${BINARY_PATH}).\nPlease compile the Acala project:\nmake test-ts\x1b[0m`
			);
		} else {
			console.error(err);
		}
		process.exit(1);
	});

	let provider = null;
	const binaryLogs = [];
	await new Promise<void>((resolve) => {
		const timer = setTimeout(() => {
			console.error(`\x1b[31m Failed to start Acala Node.\x1b[0m`);
			console.error(`Command: ${cmd} ${args.join(" ")}`);
			console.error(`Logs:`);
			console.error(binaryLogs.map((chunk) => chunk.toString()).join("\n"));
			process.exit(1);
		}, SPAWNING_TIME - 2000);

		const onData = async (chunk) => {
			if (DISPLAY_LOG) {
				console.log(chunk.toString());
			}
			binaryLogs.push(chunk);
			if (chunk.toString().match(/Listening for new connections on/)) {
				provider = new TestProvider({
					provider: new WsProvider(`ws://localhost:${WS_PORT}`),
					// TODO: add types and remove
					types: {
						TransactionAction: {
							_enum: {
								Call: "H160",
								Create: "Null",
							},
						},
						ExtrinsicSignature: {
							_enum: {
								Ed25519: "Ed25519Signature",
								Sr25519: "Sr25519Signature",
								Ecdsa: "EcdsaSignature",
								Ethereum: "[u8; 65]",
								AcalaEip712: "[u8; 65]",
							},
						},
					},
				});

				// This is needed as the EVM runtime needs to warmup with a first call
				await provider.getNetwork();

				clearTimeout(timer);
				if (!DISPLAY_LOG) {
					binary.stderr.off("data", onData);
					binary.stdout.off("data", onData);
				}
				// console.log(`\x1b[31m Starting RPC\x1b[0m`);
				resolve();
			}
		};
		binary.stderr.on("data", onData);
		binary.stdout.on("data", onData);
	});

	return { provider, binary };
}

export function describeWithAcala(title: string, cb: (context: { provider: TestProvider }) => void) {
	describe(title, () => {
		let context: { provider: TestProvider } = { provider: null };
		let binary: ChildProcess;
		// Making sure the Acala node has started
		before("Starting Acala Test Node", async function () {
			this.timeout(SPAWNING_TIME);
			const init = await startAcalaNode();
			context.provider = init.provider;
			binary = init.binary;
		});

		after(async function () {
			//console.log(`\x1b[31m Killing RPC\x1b[0m`);
			context.provider.api.disconnect()
			binary.kill();
		});

		cb(context);
	});
}

export async function nextBlock(provider: TestProvider) {
    return new Promise(async (resolve) => {
        let [ alice ] = await provider.getWallets();
        let block_number = await provider.api.query.system.number();
        provider.api.tx.system.remark(block_number.toString(16)).signAndSend(await alice.getSubstrateAddress(), (result) => {
            if (result.status.isInBlock) {
				resolve(undefined);
            }
        });
    });
}
