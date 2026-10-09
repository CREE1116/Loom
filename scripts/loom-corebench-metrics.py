"""Pure reporting calculations shared by economic benchmarks and their tests."""
from collections import defaultdict
from statistics import median


def union_ms(intervals):
    spans=sorted(intervals)
    if not spans:return 0
    start,end=spans[0];total=0
    for a,b in spans[1:]:
        if a>end:total+=end-start;start,end=a,b
        else:end=max(end,b)
    return total+end-start


def measured_sum(values):
    values=list(values)
    return sum(values) if all(v is not None for v in values) else None


def observation(row):
    report=row.get('report',{});calls=report.get('model_calls',[])
    intervals=[(c['start_ms'],c['end_ms']) for c in calls if c.get('start_ms') is not None and c.get('end_ms') is not None]
    model_wall=union_ms(intervals) if 'report' in row and len(intervals)==len(calls) else None
    validation=measured_sum(v.get('ms',v.get('duration_ms')) for v in report.get('validation',[]))
    total_tokens=measured_sum([report.get('input'),report.get('output')])
    return {'passed':bool(row.get('passed')),'worker_seconds':row.get('worker_seconds'),
        'verified_seconds':row.get('verified_seconds'),'tokens':total_tokens,
        'input':report.get('input'),'output':report.get('output'),
        'known_input':sum(c['input'] for c in calls if c.get('input') is not None),'known_output':sum(c['output'] for c in calls if c.get('output') is not None),
        'reported_usage_calls':sum(c.get('input') is not None and c.get('output') is not None for c in calls),
        'model_calls':report.get('calls'),'model_wall_ms':model_wall,
        'model_request_ms':sum(c['ms'] for c in calls),'validation_ms':validation,
        'other_ms':max(0,round(row['worker_seconds']*1000)-model_wall-validation) if model_wall is not None and validation is not None else None,
        'first_response_ms':calls[0]['ms'] if calls else None,
        'first_text_ms':next((c['first_text_ms'] for c in calls if c.get('first_text_ms') is not None),None),
        'empty_responses':sum(c.get('stop') is not None and c.get('tool_calls',0)==0 and c.get('text_chars',0)==0 for c in calls),
        'model_errors':sum(c.get('error') is not None for c in calls),
        'split':report.get('plan_mode')=='split','groups':len(report.get('groups',[]))}


def summarize(rows):
    groups=defaultdict(list)
    for row in rows:
        if row.get('eligible') is False or ('split' in row['version'] and row.get('decomposable') is False):continue
        groups[(row['requested_model'],row['version'])].append(row)
    summaries=[]
    for (model,version),group in sorted(groups.items()):
        obs=[observation(r) for r in group];wins=sum(o['passed'] for o in obs)
        tokens=measured_sum(o['tokens'] for o in obs)
        latency=[o['worker_seconds'] for o in obs if o['worker_seconds'] is not None]
        success_wall=[o['verified_seconds'] for o in obs if o['passed'] and o['verified_seconds'] is not None]
        summaries.append({'model':model,'version':version,'passed':wins,'runs':len(obs),
            'median_worker_seconds':median(latency) if latency else None,
            'median_verified_success_seconds':median(success_wall) if success_wall else None,
            'input':measured_sum(o['input'] for o in obs),'output':measured_sum(o['output'] for o in obs),
            'tokens':tokens,'token_observed_runs':sum(o['tokens'] is not None for o in obs),
            'tokens_per_success_including_failures':tokens/wins if wins and tokens is not None else None,
            'worker_seconds_per_success_including_failures':sum(latency)/wins if wins else None,
            'model_calls':measured_sum(o['model_calls'] for o in obs),
            'model_wall_ms':measured_sum(o['model_wall_ms'] for o in obs),
            'validation_ms':measured_sum(o['validation_ms'] for o in obs),
            'other_ms':measured_sum(o['other_ms'] for o in obs),
            'known_input':sum(o['known_input'] for o in obs),'known_output':sum(o['known_output'] for o in obs),
            'reported_usage_calls':sum(o['reported_usage_calls'] for o in obs),
            'split_runs':sum(o['split'] for o in obs),
            'model_errors':sum(o['model_errors'] for o in obs)})
    return summaries
