export type SessionResponse = {
	authenticated: boolean;
	username: string | null;
};

export type StatusResponse = {
	authenticated_username: string;
	bind_addr: string;
	git_store: string;
	frontend_dist: string;
	keegate_api_enabled: boolean;
	client_count: number;
	push_endpoint_count: number;
	main_branch_exists: boolean;
};

type ApiError = {
	error?: string;
	message?: string;
};

async function parseJson<T>(response: Response): Promise<T> {
	return (await response.json()) as T;
}

async function parseError(response: Response): Promise<string> {
	try {
		const body = await parseJson<ApiError>(response);
		return body.message ?? `Request failed with status ${response.status}`;
	} catch {
		return `Request failed with status ${response.status}`;
	}
}

async function request<T>(input: string, init?: RequestInit): Promise<T> {
	const response = await fetch(input, {
		credentials: 'same-origin',
		...init
	});

	if (!response.ok) {
		const error = new Error(await parseError(response));
		error.name = response.status === 401 ? 'UnauthorizedError' : 'ApiError';
		throw error;
	}

	return parseJson<T>(response);
}

export function getSession() {
	return request<SessionResponse>('/api/ui/v1/session');
}

export function login(username: string, password: string) {
	return request<SessionResponse>('/api/ui/v1/session/login', {
		method: 'POST',
		headers: {
			'content-type': 'application/json'
		},
		body: JSON.stringify({ username, password })
	});
}

export function logout() {
	return request<SessionResponse>('/api/ui/v1/session/logout', {
		method: 'POST'
	});
}

export function getStatus() {
	return request<StatusResponse>('/api/ui/v1/status');
}
