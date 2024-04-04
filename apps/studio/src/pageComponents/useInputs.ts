import { useEffect } from "react";
import { type State } from "~/schemata/stateSchemata";
import { SEARCH_PARAMS_KEYS } from "~/store/getInitialState";

import { useModStore } from "~/store/zustand/mod";
import { useSnippetStore } from "~/store/zustand/snippets";

export const useInputs = () => {
	const {
		engine,
		setEngine,
		setInput,
		setOutput,
		inputSnippet,
		outputSnippet,
	} = useSnippetStore();
	const { internalContent, setContent } = useModStore();

	useEffect(() => {
		localStorage.setItem(
			"state",
			JSON.stringify({
				engine,
				beforeSnippet: inputSnippet,
				afterSnippet: outputSnippet,
				codemodSource: internalContent ?? "",
			} satisfies State),
		);
	}, [engine, inputSnippet, outputSnippet, internalContent]);

	useEffect(() => {
		const storageEventListener = (storageEvent: StorageEvent) => {
			if (storageEvent.key === SEARCH_PARAMS_KEYS.ENGINE) {
				if (
					storageEvent.newValue === "jscodeshift" ||
					storageEvent.newValue === "tsmorph"
				) {
					setEngine(storageEvent.newValue);
					return;
				}

				setEngine("jscodeshift");
			}

			if (storageEvent.key === SEARCH_PARAMS_KEYS.AFTER_SNIPPET) {
				setInput(storageEvent.newValue ?? "");
			}

			if (storageEvent.key === SEARCH_PARAMS_KEYS.BEFORE_SNIPPET) {
				setOutput(storageEvent.newValue ?? "");
			}

			if (storageEvent.key === SEARCH_PARAMS_KEYS.CODEMOD_SOURCE) {
				setContent(storageEvent.newValue ?? "");
			}
		};

		window.addEventListener("storage", storageEventListener);

		return () => {
			window.removeEventListener("storage", storageEventListener);
		};
	}, []);
};
