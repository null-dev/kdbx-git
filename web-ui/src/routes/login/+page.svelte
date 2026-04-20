<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { get } from 'svelte/store';

	import { login as loginRequest } from '$lib/api';
	import Button from '$lib/components/ui/button/button.svelte';
	import Card from '$lib/components/ui/card/card.svelte';
	import CardContent from '$lib/components/ui/card/card-content.svelte';
	import CardDescription from '$lib/components/ui/card/card-description.svelte';
	import CardHeader from '$lib/components/ui/card/card-header.svelte';
	import CardTitle from '$lib/components/ui/card/card-title.svelte';
	import Input from '$lib/components/ui/input/input.svelte';
	import Label from '$lib/components/ui/label/label.svelte';
	import { refreshSession, session } from '$lib/session';

	let username = $state('');
	let password = $state('');
	let isSubmitting = $state(false);
	let errorMessage = $state<string | null>(null);

	onMount(async () => {
		const currentSession = await refreshSession();
		if (currentSession.authenticated) {
			await goto('/');
		}
	});

	async function submitLogin() {
		errorMessage = null;
		isSubmitting = true;

		try {
			await loginRequest(username, password);
			await refreshSession();
			await goto('/');
		} catch (error) {
			errorMessage = error instanceof Error ? error.message : 'Login failed.';
		} finally {
			isSubmitting = false;
		}
	}
</script>

<div class="mx-auto flex min-h-[calc(100vh-10rem)] max-w-5xl items-center">
	<div class="grid w-full gap-6 lg:grid-cols-[1.05fr_0.95fr]">
		<section class="flex flex-col justify-center gap-4">
			<p class="text-sm font-medium uppercase tracking-[0.22em] text-primary">
				kdbx-git operator console
			</p>
			<h1 class="max-w-xl text-4xl font-semibold tracking-tight text-balance">
				A clean admin shell for branch-backed KeePass sync.
			</h1>
			<p class="max-w-xl text-base leading-7 text-muted-foreground">
				Milestone 1 brings the first web login, app shell, and status dashboard online. The
				Rust server still owns the real auth and API logic.
			</p>
		</section>

		<Card class="mx-auto w-full max-w-lg">
			<CardHeader>
				<CardTitle>Admin login</CardTitle>
				<CardDescription>
					Use a configured `web_ui.admin_users` account to access the dashboard.
				</CardDescription>
			</CardHeader>
			<CardContent class="space-y-5">
				<div class="space-y-2">
					<Label for="username">Username</Label>
					<Input
						id="username"
						name="username"
						autocomplete="username"
						bind:value={username}
						placeholder="admin"
					/>
				</div>

				<div class="space-y-2">
					<Label for="password">Password</Label>
					<Input
						id="password"
						name="password"
						type="password"
						autocomplete="current-password"
						bind:value={password}
						placeholder="Enter your password"
					/>
				</div>

				{#if errorMessage}
					<div class="rounded-2xl border border-destructive/30 bg-destructive/8 px-4 py-3 text-sm text-destructive">
						{errorMessage}
					</div>
				{/if}

				<Button class="w-full" disabled={isSubmitting || !username || !password} onclick={submitLogin}>
					{isSubmitting ? 'Signing in...' : 'Sign in'}
				</Button>
			</CardContent>
		</Card>
	</div>
</div>
