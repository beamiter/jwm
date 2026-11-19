#!/usr/bin/env python3
"""Offline contract checks only; no X display or product binary runs."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import MagicMock,patch
import product_probe as p

class Contracts(unittest.TestCase):
    def session(self):
        s=p.Session.__new__(p.Session); s.same_tag=False; s.windows={'A':101,'B':102};s.raw_xids={'A':8388609,'B':8388610}
        s.b={'num':1,'active_tags':64,'layout':'LayoutEnum("float")','n_master':4,'focused':True,'selected_id':102}
        s.b_workspace={'layout':'LayoutEnum("float")','n_master':4,'m_fact':.55,'gap':8}
        snap={'get_monitors':[{'name':'DUMMY1',**s.b}],
              'get_workspaces':[{'monitor':1,'tag_mask':64,**s.b_workspace}],
              'get_windows':[{'id':101,'monitor':1,'tags':4,'is_on_view':False,'is_focused':False},
                             {'id':102,'monitor':1,'tags':64,'is_on_view':True,'is_focused':True}],
              'input_focus':8388610}
        return s,snap
    def test_correct_snapshot(self):
        s,snap=self.session(); self.assertEqual(s.differences(snap),[])
    def test_wrong_input_recipient_is_failure(self):
        s,snap=self.session(); snap['input_focus']=101
        self.assertIn('input_focus',[d['field'] for d in s.differences(snap)])
    def test_workspace_loss_is_detected(self):
        s,snap=self.session(); snap['get_workspaces'][0]['layout']='TILE'
        self.assertIn('workspace-layout',[d['field'] for d in s.differences(snap)])
    def test_lost_client_is_detected(self):
        s,snap=self.session(); snap['get_windows'].pop()
        self.assertIn('window-B',[d['field'] for d in s.differences(snap)])
    def test_same_tag_visibility(self):
        s,snap=self.session(); s.same_tag=True
        self.assertTrue(s.differences(snap))
        snap['get_windows'][0].update(tags=64,is_on_view=True)
        self.assertEqual(s.differences(snap),[])
    def test_cleanup_failure_not_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake=MagicMock(); fake.probe.return_value={'status':'passed','result':'fixed-real-events-passed'}
            fake.close.side_effect=RuntimeError('cleanup')
            fake.phase='test'; fake.server=None
            with patch.object(p,'Session',return_value=fake):
                r=p.execute(Path('/unused'),Path('/unused'),Path(tmp)/'result',p.FIXED,False,{})
            self.assertEqual(r['status'],'failed')
    def test_setup_failure_not_baseline_reproduction(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake=MagicMock(); fake.probe.side_effect=RuntimeError('no socket'); fake.phase='setup'; fake.server=None
            with patch.object(p,'Session',return_value=fake):
                r=p.execute(Path('/unused'),Path('/unused'),Path(tmp)/'result',p.BASE,False,{})
            self.assertEqual(r['status'],'failed'); self.assertNotIn('result',r)

    def test_body_and_cleanup_errors_both_retained(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake=MagicMock(); fake.probe.side_effect=RuntimeError('body'); fake.close.side_effect=RuntimeError('close')
            fake.phase='probe'; fake.server=None
            with patch.object(p,'Session',return_value=fake):
                r=p.execute(Path('/unused'),Path('/unused'),Path(tmp)/'result',p.BASE,False,{})
            self.assertEqual(len(r['errors']),2); self.assertEqual(r['status'],'failed')
    def test_wrong_checkout_rejected_before_any_source_or_binary_execution(self):
        with tempfile.TemporaryDirectory() as tmp:
            calls=[]
            def git(argv,**kwargs):
                calls.append(argv)
                return tmp if '--show-toplevel' in argv else 'wrongsha'
            with patch.object(p.subprocess,'check_output',side_effect=git), \
                 patch.object(p.subprocess,'Popen',side_effect=AssertionError('must not execute')):
                with self.assertRaisesRegex(RuntimeError,'wrong source revision'):
                    p.Session(Path(tmp),Path('/unused'),Path(tmp),Path(tmp),p.BASE,False,{})
            self.assertEqual(len(calls),2)

    def test_dirty_checkout_rejected_before_execution(self):
        with tempfile.TemporaryDirectory() as tmp:
            responses=iter([tmp,p.BASE,' M src/jwm.rs'])
            with patch.object(p.subprocess,'check_output',side_effect=lambda *a,**k:next(responses)), \
                 patch.object(p.subprocess,'Popen',side_effect=AssertionError('must not execute')):
                with self.assertRaisesRegex(RuntimeError,'not clean'):
                    p.Session(Path(tmp),Path('/unused'),Path(tmp),Path(tmp),p.BASE,False,{})
    def test_failed_build_receipt_rejected(self):
        s=p.Session.__new__(p.Session);s.root=Path('/source');s.expected=p.BASE
        s.build={'source':p.BASE,'exit_code':1}
        with patch.object(p,'checked_source'),patch.object(p.subprocess,'Popen',side_effect=AssertionError('must not execute')):
            with self.assertRaisesRegex(RuntimeError,'successful exact build'): s.start()
    def test_changed_binary_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);binary=root/'jwm';binary.write_text('changed')
            s=p.Session.__new__(p.Session);s.root=root;s.expected=p.BASE;s.bin=binary
            s.build={'source':p.BASE,'exit_code':0,'root':str(root),'binary':str(binary),'binary_sha256':'0'*64}
            with patch.object(p,'checked_source'),patch.object(p.subprocess,'Popen',side_effect=AssertionError('must not execute')):
                with self.assertRaisesRegex(RuntimeError,'binary changed'): s.start()

    def test_real_artifact_separates_id_domains(self):
        evidence=json.loads((Path(__file__).parent/'fixtures/initial-730c2705.json').read_text())
        row=evidence['ipc_window'];raw=evidence['native_xid_from_jwm_log']
        self.assertEqual(row['id'],4);self.assertEqual(raw,8388609)
        self.assertEqual([w for w in [row] if w['id']==raw],[])  # former timeout cause
        self.assertEqual(p.match_ipc_client([row],11686,'org.jwm.randrfixturea')['id'],4)
        self.assertEqual(p.layout_name(row['layout']),'tile')
    def test_wrong_and_ambiguous_ipc_identity_rejected(self):
        row={'id':4,'pid':11686,'name':'fixture','class':'fixture','instance':'fixture'}
        self.assertIsNone(p.match_ipc_client([row],999,'fixture'))
        for key in ('name','class','instance'):
            self.assertIsNone(p.match_ipc_client([{**row,key:'other'}],11686,'fixture'))
        with self.assertRaisesRegex(RuntimeError,'ambiguous'):
            p.match_ipc_client([row,{**row,'id':5}],11686,'fixture')
    def test_layout_contract_is_exact(self):
        self.assertEqual(p.layout_name('LayoutEnum("float")'),'float')
        for value in ('FLOAT','float','junkLayoutEnum("float")','LayoutEnum("float")suffix','LayoutEnum("fibonacci")'):
            with self.assertRaisesRegex(RuntimeError,'unexpected exact'):p.layout_name(value)
    def test_native_property_identity_is_exact(self):
        text='WM_NAME(STRING) = "fixture"\nWM_CLASS(STRING) = "fixture", "fixture"\n_NET_WM_PID(CARDINAL) = 11686\n'
        p.native_properties(text,11686,'fixture')
        for altered in (text.replace('11686','9'),text.replace('"fixture",','"other",'),text+text.splitlines()[0]+'\n'):
            with self.assertRaises(RuntimeError):p.native_properties(altered,11686,'fixture')
    def test_receipt_requires_native_not_abstract_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'receipt'
            path.write_text(json.dumps({'window':8388609,'send_event':0,'text_hex':'6162'})+'\n')
            self.assertEqual(p.event_text(p.key_events(path,8388609)),b'ab')
            with self.assertRaisesRegex(RuntimeError,'unexpected native'):p.key_events(path,4)
            path.write_text(json.dumps({'window':8388609,'send_event':1,'text_hex':'6162'})+'\n')
            with self.assertRaisesRegex(RuntimeError,'unexpected native'):p.key_events(path,8388609)

if __name__=='__main__': unittest.main()
