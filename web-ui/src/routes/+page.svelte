<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { get } from 'svelte/store';

	import { getStatus, type StatusResponse } from '$lib/api';
	import Badge from '$lib/components/ui/badge/badge.svelte';
	import Card from '$lib/components/ui/card/card.svelte';
	import CardContent from '$lib/components/ui/card/card-content.svelte';
	import CardDescription from '$lib/components/ui/card/card-description.svelte';
	import CardHeader from '$lib/components/ui/card/card-header.svelte';
	import CardTitle from '$lib/components/ui/card/card-title.svelte';
	import { refreshSession, session } from '$lib/session';

	let status = $state<StatusResponse | null>(null);
	let loading = $state(true);
	let errorMessage = $state<string | null>(null);

	onMount(async () => {
		const currentSession = await refreshSession();

		if (!currentSession.authenticated) {
			await goto('/login');
			return;
		}

		try {
			status = await getStatus();
		} catch (error) {
			if (error instanceof Error && error.name === 'UnauthorizedError') {
				await goto('/login');
				return;
			}
			errorMessage = error instanceof Error ? error.message : 'Failed to load server status.';
		} finally {
			loading = false;
		}
	});

	const cards = $derived(
		status
			? [
					{
						label: 'Clients',
						value: status.client_count.toString(),
						description: 'Configured WebDAV/sync identities'
					},
					{
						label: 'Push Endpoints',
						value: status.push_endpoint_count.toString(),
						description: 'Registered mobile subscriptions'
					},
					{
						label: 'KeeGate API',
						value: status.keegate_api_enabled ? 'Enabled' : 'Disabled',
						description: 'Read-only secrets API availability'
					},
					{
						label: 'Main Branch',
						value: status.main_branch_exists ? 'Present' : 'Missing',
						description: 'Whether canonical history has been initialized'
					}
				]
			: []
	);
</script>

<div class="flex flex-col gap-6">
	<section class="grid gap-4 lg:grid-cols-[1.35fr_0.65fr]">
		<Card class="overflow-hidden">
			<CardHeader class="gap-3">
				<div class="flex items-center justify-between gap-3">
					<div>
						<CardTitle class="text-2xl">Server overview</CardTitle>
						<CardDescription>
							A first-pass admin dashboard for the `kdbx-git` server.
						</CardDescription>
					</div>
					<Badge variant="outline">
						{#if $session.authenticated}
							Authenticated
						{:else}
							Guest
						{/if}
					</Badge>
				</div>
			</CardHeader>
			<CardContent class="space-y-4">
				{#if loading}
					<div class="grid gap-3 md:grid-cols-2">
						{#each Array.from({ length: 4 }) as _, index}
							<div class="rounded-2xl border border-border/70 bg-secondary/70 p-4" aria-hidden="true">
								<div class="h-3 w-20 rounded-full bg-muted"></div>
								<div class="mt-4 h-7 w-28 rounded-full bg-muted"></div>
								<div class="mt-3 h-3 w-40 rounded-full bg-muted"></div>
							</div>
						{/each}
					</div>
				{:else if errorMessage}
					<div class="rounded-2xl border border-destructive/30 bg-destructive/8 p-4 text-sm text-destructive">
						{errorMessage}
					</div>
				{:else if status}
					<div class="grid gap-3 md:grid-cols-2">
						{#each cards as card}
							<div class="rounded-2xl border border-border/70 bg-background/80 p-4">
								<p class="text-sm text-muted-foreground">{card.label}</p>
								<p class="mt-2 text-2xl font-semibold tracking-tight">{card.value}</p>
								<p class="mt-2 text-sm text-muted-foreground">{card.description}</p>
							</div>
						{/each}
					</div>
				{/if}
			</CardContent>
		</Card>

		<Card>
			<CardHeader>
				<CardTitle>Milestone 1 scope</CardTitle>
				<CardDescription>
					This UI currently covers authentication, shell layout, and a status dashboard.
				</CardDescription>
			</CardHeader>
			<CardContent class="space-y-3 text-sm text-muted-foreground">
				<p>Next milestones can add client details, history, push health, and the KeeGate browser.</p>
				<div class="rounded-2xl border border-border/70 bg-secondary/60 p-4">
					<p class="font-medium text-foreground">Integrated delivery</p>
					<p class="mt-1">The built Svelte app is served directly by the Rust server under `/ui`.</p>
				</div>
			</CardContent>
		</Card>
	</section>

	{#if status}
		<section class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
			<Card>
				<CardHeader class="pb-3">
					<CardTitle class="text-base">Bind address</CardTitle>
				</CardHeader>
				<CardContent class="text-sm text-muted-foreground">{status.bind_addr}</CardContent>
			</Card>

			<Card>
				<CardHeader class="pb-3">
					<CardTitle class="text-base">Git store</CardTitle>
				</CardHeader>
				<CardContent class="break-all text-sm text-muted-foreground">{status.git_store}</CardContent>
			</Card>

			<Card>
				<CardHeader class="pb-3">
					<CardTitle class="text-base">Asset delivery</CardTitle>
				</CardHeader>
				<CardContent class="text-sm text-muted-foreground">{status.asset_delivery}</CardContent>
			</Card>

			<Card>
				<CardHeader class="pb-3">
					<CardTitle class="text-base">Admin session</CardTitle>
				</CardHeader>
				<CardContent class="text-sm text-muted-foreground">
					{status.authenticated_username}
				</CardContent>
			</Card>
		</section>
	{/if}
</div>
