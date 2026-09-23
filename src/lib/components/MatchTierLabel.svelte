<!-- A match group's confidence, as its tier letter. Shared by the Deduplicate
     view's group list and `GroupMembers`, so the two cannot drift. Never shown
     as a percentage: percentages in the UI mean duplication shares. -->
<script lang="ts">
    import { confidenceTone, matchTier } from '$lib/util';

    interface Props {
        confidence: number;
        /** The group's kind; a folder's tier is an average over its files. */
        kind: string;
    }

    let { confidence, kind }: Props = $props();

    const t = $derived(matchTier(confidence));
    const title = $derived(
        `Tier ${t.tier} · ${t.label}` +
            (kind === 'folder'
                ? '. For a folder: the matches inside it are, on average, at least this strong.'
                : '')
    );
</script>

<span class="font-heading font-semibold {confidenceTone(t.confidence)}" {title}
    >Tier {t.tier}</span>
