const eventManager = new globalThis.EventWorker();

console.log('event worker running');

for await (const data of eventManager) {
	if (data) {
		switch (data.event_type) {
			case 'Log':
				if (data.event.level === 'Error') {
					console.error(data.event.msg);
				} else {
					console.log(data.event.msg);
				}
				break;
			case 'UncaughtException':
				console.error(data.event.exception);
				break;
			default:
				console.log(data);
		}
	}
}
