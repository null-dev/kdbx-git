<script lang="ts">
	import { cn } from '$lib/utils';

	let { class: className = '', children } = $props<{
		class?: string;
		children?: import('svelte').Snippet;
	}>();
</script>

<div class={cn('px-6 pb-6', className)}>
	{@render children?.()}
</div>
