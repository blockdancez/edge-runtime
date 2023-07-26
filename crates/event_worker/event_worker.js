const primordials = globalThis.__bootstrap.primordials;
const { SymbolAsyncIterator } = primordials;
const core = globalThis.Deno.core;

class EventWorker {
	async nextEvent() {
		try {
			const reqEvt = await core.opAsync('op_event_accept');
			const done = reqEvt === 'Done';

			let value = undefined;
			if (!done) {
				const rawEvent = reqEvt['Event'];
				const eventType = Object.keys(rawEvent.event)[0];
				value = {
					timestamp: new Date().toISOString(),
					event_type: eventType,
					event: rawEvent.event[eventType],
					metadata: rawEvent.metadata,
				};
			}

			return { value, done };
		} catch (e) {
			// TODO: handle errors
			throw e;
		}
	}

	[SymbolAsyncIterator]() {
		const scopedClass = this;

		return {
			async next() {
				return await scopedClass.nextEvent();
			},
		};
	}
}

export { EventWorker };
