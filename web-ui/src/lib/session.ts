import { writable } from 'svelte/store';

import { getSession, type SessionResponse } from '$lib/api';

const defaultSession: SessionResponse = {
	authenticated: false,
	username: null
};

export const session = writable<SessionResponse>(defaultSession);
export const sessionReady = writable(false);

export async function refreshSession() {
	try {
		const nextSession = await getSession();
		session.set(nextSession);
		return nextSession;
	} catch {
		session.set(defaultSession);
		return defaultSession;
	} finally {
		sessionReady.set(true);
	}
}

export function clearSession() {
	session.set(defaultSession);
	sessionReady.set(true);
}
