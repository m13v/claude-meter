import json, subprocess, urllib.request
def kc(s): return subprocess.run(["security","find-generic-password","-s",s,"-w"],capture_output=True,text=True).stdout.strip()
PH=kc("posthog-personal-api-key")
def hq(sql):
    body=json.dumps({"query":{"kind":"HogQLQuery","query":sql}}).encode()
    req=urllib.request.Request("https://us.posthog.com/api/projects/22841/query/",data=body,headers={"Authorization":"Bearer "+PH,"Content-Type":"application/json"})
    return json.load(urllib.request.urlopen(req))["results"]
lines=["POSTHOG daily_active (server-side active-install) per day:"]
for d,u,e in hq("select toDate(timestamp),count(distinct distinct_id),count() from events where event='daily_active' and timestamp>now()-interval 9 day group by 1 order by 1"):
    lines.append("  %s  installs=%d  events=%d"%(d,u,e))
lines.append("")
lines.append("POSTHOG daily_active NEW installs (first daily_active ever) per day:")
for d,n in hq("select toDate(t),count() from (select distinct_id,min(timestamp) t from events where event='daily_active' group by distinct_id) where t>now()-interval 9 day group by 1 order by 1"):
    lines.append("  %s  new=%d"%(d,n))
open("/tmp/d_out.txt","w").write("\n".join(lines)+"\n")
print("WROTE", len(lines), "lines")
