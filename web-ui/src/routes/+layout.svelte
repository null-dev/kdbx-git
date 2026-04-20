<script lang="ts">
	import './layout.css';

	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import favicon from '$lib/assets/favicon.svg';
	import Button from '$lib/components/ui/button/button.svelte';
	import { logout as logoutRequest } from '$lib/api';
	import { clearSession, refreshSession, session, sessionReady } from '$lib/session';

	let { children } = $props();

	let isLoggingOut = $state(false);
	const isLoginRoute = $derived(page.url.pathname.endsWith('/login'));

	$effect(() => {
		if (!$sessionReady) {
			void refreshSession();
		}
	});

	async function logout() {
		isLoggingOut = true;

		try {
			await logoutRequest();
		} finally {
			clearSession();
			isLoggingOut = false;
			await goto('/login');
		}
	}
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<title>kdbx-git Web UI</title>
</svelte:head>

<div class="min-h-screen">
	<header class="sticky top-0 z-20 border-b border-border/60 bg-background/75 backdrop-blur-xl">
		<div class="mx-auto flex max-w-6xl items-center justify-between gap-4 px-4 py-4 sm:px-6">
			<div class="flex items-center gap-3">
				<div class="rounded-2xl bg-primary/12 px-3 py-2 text-sm font-semibold text-primary">
					kdbx-git
				</div>
				<div>
					<p class="text-sm font-semibold tracking-tight">Web UI</p>
					<p class="text-xs text-muted-foreground">Milestone 1 admin shell</p>
				</div>
			</div>

			<nav class="flex items-center gap-2">
				<a
					href="/ui/"
					class="rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-secondary hover:text-foreground"
				>
					Dashboard
				</a>

				{#if $session.authenticated}
					<div class="hidden text-sm text-muted-foreground sm:block">
						Signed in as <span class="font-medium text-foreground">{$session.username}</span>
					</div>
					<Button variant="outline" disabled={isLoggingOut} onclick={logout}>
						{isLoggingOut ? 'Signing out...' : 'Logout'}
					</Button>
				{:else if !isLoginRoute}
					<a
						href="/ui/login"
						class="rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-secondary hover:text-foreground"
					>
						Login
					</a>
				{/if}
			</nav>
		</div>
	</header>

	<main class="mx-auto max-w-6xl px-4 py-8 sm:px-6">
		{@render children()}
	</main>
</div>
